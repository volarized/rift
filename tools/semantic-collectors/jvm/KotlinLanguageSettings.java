import java.nio.file.Path;
import java.util.List;

public record KotlinLanguageSettings(List<String> optIns, String languageVersion, String apiVersion,
    String jvmTarget, List<Path> friendPaths) {
    static final KotlinLanguageSettings DEFAULT = new KotlinLanguageSettings(List.of(), null, null, null, List.of());

    public KotlinLanguageSettings {
        optIns = List.copyOf(optIns);
        friendPaths = List.copyOf(friendPaths);
    }
}
