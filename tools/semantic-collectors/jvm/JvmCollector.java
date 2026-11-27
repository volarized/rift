import com.google.gson.annotations.SerializedName;
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileVisitResult;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.SimpleFileVisitor;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeSet;
import javax.lang.model.SourceVersion;

public final class JvmCollector {
    private static final int VERSION = 1;
    private static final int SOURCE_BYTES_MAX = 1 << 20;
    private static final int SUPPORT_BYTES_MAX = 4 << 20;
    private static final int SUPPORT_COUNT_MAX = 512;
    private static final int OBSERVATIONS_MAX = 16_384;

    enum Coverage {
        @SerializedName("complete") COMPLETE,
        @SerializedName("partial") PARTIAL,
        @SerializedName("unsupported") UNSUPPORTED
    }

    enum Evidence {
        @SerializedName("resolution") RESOLUTION,
        @SerializedName("unresolved") UNRESOLVED
    }

    record Anchor(String unit, String sourceSha256, Facts.Range range) {}
    record Observation(String id, Facts.Range occurrence, Facts.Range enclosing, Anchor target,
        Evidence evidence) {}
    record Batch(Coverage coverage, List<Observation> observations) {}

    private enum Language {
        JAVA(".java"), KOTLIN(".kt");

        private final String suffix;
        Language(String suffix) {
            this.suffix = suffix;
        }

        boolean matches(String filename) {
            return filename.endsWith(suffix) && filename.length() > suffix.length();
        }
    }

    private final CollectorProtocol protocol;
    private final Options options;
    private final GradleContext gradle;
    private KotlinCompiler kotlin;

    private JvmCollector(CollectorProtocol protocol, Options options) throws IOException {
        this.protocol = protocol;
        this.options = options;
        gradle = GradleContext.read(options.gradleContext());
    }

    public static void main(String[] arguments) throws Exception {
        var protocol = new CollectorProtocol(System.in, System.out);
        System.setOut(System.err);
        var options = Options.parse(arguments);
        if (options.kotlinHome() != null) {
            requireRestrictedJvm();
        }
        var collector = new JvmCollector(protocol, options);
        for (var request = protocol.next(); request != null; request = protocol.next()) {
            protocol.reply(request.id(), collector.extract(request.params()));
        }
    }

    private static void requireRestrictedJvm() {
        String prefix = "--sun-misc-unsafe-memory-access=";
        var policy = ManagementFactory.getRuntimeMXBean().getInputArguments().stream()
            .filter(argument -> argument.startsWith(prefix))
            .reduce((first, last) -> last);
        if (policy.filter((prefix + "deny")::equals).isEmpty()
            || ModuleLayer.boot().findModule("jdk.unsupported").isPresent()) {
            throw new IllegalArgumentException("Kotlin collection requires restricted JVM options: "
                + "--sun-misc-unsafe-memory-access=deny "
                + "and --limit-modules=java.se,jdk.compiler,jdk.zipfs,jdk.management");
        }
    }

    private Batch extract(CollectorProtocol.Extraction request) throws Exception {
        Integer limit = request.observationsMax();
        if (request.version() != VERSION || request.category() == null || limit == null
            || limit < 0 || limit > OBSERVATIONS_MAX || request.source() == null) {
            throw new IOException("invalid extraction parameters");
        }
        var selected = request.source();
        String name = filename(selected);
        Language language;
        if (Language.JAVA.matches(name)) {
            language = Language.JAVA;
        } else if (Language.KOTLIN.matches(name) && options.kotlinHome() != null) {
            language = Language.KOTLIN;
        } else {
            return new Batch(Coverage.UNSUPPORTED, List.of());
        }

        var scope = gradle.scope(selected.unit());
        var supports = new LinkedHashSet<>(scope.support());
        supports.addAll(options.support());
        supports.remove(selected.unit());
        var classpath = new LinkedHashSet<>(scope.classpath());
        classpath.addAll(options.classpath());
        var libraries = List.copyOf(classpath);
        try (var staging = new Staging(Files.createTempDirectory("rift-jvm-"), language)) {
            var input = AdmitSource.from(selected, language);
            staging.add(input);
            int supportBytes = 0;
            int supportCount = 0;
            boolean partial = false;
            for (String unit : supports) {
                if (++supportCount > SUPPORT_COUNT_MAX) {
                    partial = true;
                    break;
                }
                var source = protocol.supportingSource(unit);
                if (source == null) {
                    partial = true;
                    continue;
                }
                String filename = filename(source);
                if (!Language.JAVA.matches(filename)
                    && (language != Language.KOTLIN || !Language.KOTLIN.matches(filename))) {
                    continue;
                }

                var supporting = AdmitSource.from(source, language);
                supportBytes += supporting.offsets().bytes();
                if (supportBytes > SUPPORT_BYTES_MAX) {
                    partial = true;
                    break;
                }
                staging.add(supporting);
            }

            var facts = new Facts(request.category(), limit, staging.paths);
            if (language == Language.JAVA) {
                Path module = staging.root.resolve("module");
                JavacFacts.collect(facts, staging.paths, libraries, staging.root,
                    Files.isDirectory(module) ? options.javaModule() + "=" + module : null);
            } else {
                if (kotlin == null) {
                    kotlin = ServiceLoader.load(KotlinCompiler.class).findFirst()
                        .orElseThrow(() -> new IOException("Kotlin compiler is unavailable"));
                }
                kotlin.collect(facts, staging.paths, libraries, options.kotlinHome(),
                    staging.root.resolve("classes"), options.kotlinStdlib(), scope.kotlin());
            }
            return staging.batch(facts, partial);
        }
    }

    private static String filename(CollectorProtocol.Source source) throws IOException {
        if (source.filename() == null) {
            throw new IOException("missing source filename");
        }
        String name = source.filename().replace('\\', '/');
        return name.substring(name.lastIndexOf('/') + 1);
    }

    private record AdmitSource(CollectorProtocol.Source source, String filename, SourceOffsets offsets) {
        static AdmitSource from(CollectorProtocol.Source source, Language language) throws IOException {
            String name = JvmCollector.filename(source);
            if (name.indexOf(':') >= 0 || name.chars().anyMatch(character -> character < 32)) {
                throw new IOException("invalid compiler source filename");
            }
            if (source.text() == null || source.unit() == null || source.sha256() == null) {
                throw new IOException("missing source content or identity");
            }
            var offsets = new SourceOffsets(source.text(), language == Language.KOTLIN);
            if (offsets.bytes() > SOURCE_BYTES_MAX) {
                throw new IOException("source exceeds its byte bound");
            }
            return new AdmitSource(source, name, offsets);
        }

        Anchor anchor(Facts.Range range) throws IOException {
            return new Anchor(source.unit(), source.sha256(), offsets.bytes(range));
        }
    }

    private final class Staging implements AutoCloseable {
        private final Path root;
        private final Language language;
        private final List<AdmitSource> sources = new ArrayList<>();
        private final List<Path> paths = new ArrayList<>();

        Staging(Path root, Language language) throws IOException {
            this.root = root.toRealPath();
            this.language = language;
        }

        void add(AdmitSource input) throws IOException {
            boolean patched = language == Language.JAVA && options.moduleSources().contains(input.source().unit());
            Path folder = root.resolve(patched ? "module" : "source").resolve(Integer.toString(paths.size()));
            Files.createDirectories(folder);
            Path path = folder.resolve(input.filename());
            String text = input.source().text();
            if (language == Language.KOTLIN) {
                text = text.replace("\r\n", "\n").replace('\r', '\n');
            }
            Files.writeString(path, text, StandardCharsets.UTF_8);
            paths.add(path);
            sources.add(input);
        }

        Batch batch(Facts facts, boolean partial) throws IOException {
            var selected = sources.get(0).offsets();
            var observations = new ArrayList<Observation>();
            for (var fact : facts.observations()) {
                Anchor target = fact.target() == null ? null
                    : sources.get(fact.target().source()).anchor(fact.target().range());
                observations.add(new Observation(Integer.toString(observations.size()),
                    selected.bytes(fact.occurrence()), selected.bytes(fact.enclosing()), target,
                    target == null ? Evidence.UNRESOLVED : Evidence.RESOLUTION));
            }
            return new Batch(partial || facts.partial ? Coverage.PARTIAL : Coverage.COMPLETE, observations);
        }

        @Override
        public void close() throws IOException {
            Files.walkFileTree(root, new SimpleFileVisitor<>() {
                @Override
                public FileVisitResult visitFile(Path file, BasicFileAttributes attributes) throws IOException {
                    Files.delete(file);
                    return FileVisitResult.CONTINUE;
                }

                @Override
                public FileVisitResult postVisitDirectory(Path directory, IOException error) throws IOException {
                    if (error != null) {
                        throw error;
                    }
                    Files.delete(directory);
                    return FileVisitResult.CONTINUE;
                }
            });
        }
    }

    private record Options(List<Path> classpath, List<String> support, Set<String> moduleSources,
        Path kotlinHome, boolean kotlinStdlib, String javaModule, Path gradleContext) {
        static Options parse(String[] args) throws IOException {
            var classpath = new ArrayList<Path>();
            var support = new LinkedHashSet<String>();
            var moduleSources = new TreeSet<String>();
            Path kotlinHome = null;
            Path gradleContext = null;
            boolean kotlinStdlib = false;
            String javaModule = null;
            for (int index = 0; index < args.length; index++) {
                String option = args[index];
                if (option.equals("--kotlin-stdlib")) {
                    kotlinStdlib = true;
                    continue;
                }
                if (++index == args.length) {
                    throw new IllegalArgumentException("missing value for " + option);
                }
                String value = args[index];
                switch (option) {
                    case "--classpath" -> classpath.add(Path.of(value).toRealPath());
                    case "--source-unit" -> support.add(value);
                    case "--java-module-source" -> moduleSources.add(value);
                    case "--kotlin-home" -> kotlinHome = Path.of(value).toRealPath();
                    case "--gradle-context" -> gradleContext = Path.of(value).toRealPath();
                    case "--java-module" -> javaModule = value;
                    default -> throw new IllegalArgumentException("unknown collector option " + option);
                }
            }
            if (kotlinStdlib && kotlinHome == null) {
                throw new IllegalArgumentException("--kotlin-stdlib requires --kotlin-home");
            }
            if ((javaModule == null) != moduleSources.isEmpty()) {
                throw new IllegalArgumentException("--java-module and --java-module-source require each other");
            }
            if (javaModule != null && !SourceVersion.isName(javaModule)) {
                throw new IllegalArgumentException("invalid Java module name");
            }
            support.addAll(moduleSources);
            if (support.size() > SUPPORT_COUNT_MAX) {
                throw new IllegalArgumentException("supporting source count exceeds its bound");
            }
            return new Options(List.copyOf(classpath), List.copyOf(support), Set.copyOf(moduleSources),
                kotlinHome, kotlinStdlib, javaModule, gradleContext);
        }
    }
}
