import java.nio.file.Path;
import java.util.List;

public interface KotlinCompiler {
    void collect(Facts facts, List<Path> paths, List<Path> classpath, Path home, Path output,
        boolean stdlib) throws Exception;
}
