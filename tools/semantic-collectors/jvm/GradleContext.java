import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Pattern;

final class GradleContext {
    private static final int BYTES_MAX = 8 << 20;
    private static final Pattern VERSION = Pattern.compile("[0-9]+(?:\\.[0-9]+)?");
    private static final String IDENTIFIER = "[\\p{L}_][\\p{L}\\p{Nd}_]*";
    private static final Pattern OPT_IN = Pattern.compile(IDENTIFIER + "(?:\\." + IDENTIFIER + ")*");
    private static final Scope DEFAULT = new Scope(List.of(), List.of(), KotlinLanguageSettings.DEFAULT);

    record Scope(List<String> support, List<Path> classpath, KotlinLanguageSettings kotlin) {}
    private record Report(List<Compilation> compilations) {}
    private record Compilation(String task, List<String> selected, List<String> support,
        List<String> classpath, KotlinOptions kotlin) {}
    private record KotlinOptions(List<String> optIns, String languageVersion, String apiVersion,
        String jvmTarget, List<String> friendPaths) {
        KotlinLanguageSettings settings() {
            if (optIns.stream().anyMatch(optIn -> !OPT_IN.matcher(optIn).matches())
                || !version(languageVersion) || !version(apiVersion) || !version(jvmTarget)) {
                throw new IllegalArgumentException("invalid Gradle Kotlin compiler settings");
            }
            return new KotlinLanguageSettings(optIns, languageVersion, apiVersion, jvmTarget, paths(friendPaths));
        }

        private static boolean version(String value) {
            return value == null || VERSION.matcher(value).matches();
        }
    }

    private final Map<String, Scope> scopes;
    private final boolean supplied;

    private GradleContext(Map<String, Scope> scopes, boolean supplied) {
        this.scopes = Map.copyOf(scopes);
        this.supplied = supplied;
    }

    static GradleContext read(Path path) throws IOException {
        if (path == null) {
            return new GradleContext(Map.of(), false);
        }
        Report report;
        try (var input = Files.newInputStream(path)) {
            byte[] bytes = input.readNBytes(BYTES_MAX + 1);
            if (bytes.length > BYTES_MAX) {
                throw new IOException("Gradle context exceeds its byte bound");
            }
            String text = StandardCharsets.UTF_8.newDecoder().decode(ByteBuffer.wrap(bytes)).toString();
            report = CollectorProtocol.JSON.fromJson(text, Report.class);
        }
        var scopes = new HashMap<String, Scope>();
        for (Compilation compilation : report.compilations()) {
            var kotlin = compilation.kotlin() == null ? KotlinLanguageSettings.DEFAULT : compilation.kotlin().settings();
            var scope = new Scope(List.copyOf(compilation.support()), paths(compilation.classpath()), kotlin);
            for (String unit : compilation.selected()) {
                if (scopes.putIfAbsent(unit, scope) != null) {
                    throw new IOException("ambiguous Gradle compilation for " + unit + " in " + compilation.task());
                }
            }
        }
        return new GradleContext(scopes, true);
    }

    private static List<Path> paths(List<String> paths) {
        return paths.stream().map(Path::of).toList();
    }

    Scope scope(String unit) throws IOException {
        if (!supplied) {
            return DEFAULT;
        }
        Scope scope = scopes.get(unit);
        if (scope == null) {
            throw new IOException("no Gradle compilation for " + unit);
        }
        return scope;
    }
}
