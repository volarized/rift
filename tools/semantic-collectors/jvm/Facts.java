import com.google.gson.annotations.SerializedName;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

public final class Facts {
    enum Category {
        @SerializedName("calls") CALLS,
        @SerializedName("references") REFERENCES,
        @SerializedName("imports") IMPORTS
    }

    record Range(int start, int end) {
        Range {
            if (start < 0 || end <= start) {
                throw new IllegalArgumentException("invalid source range");
            }
        }
    }

    record Anchor(int source, Range range) {}

    record Observation(Range occurrence, Range enclosing, Anchor target) {}

    final Category category;
    final Map<Path, Integer> sources = new HashMap<>();
    private final int limit;
    private final Set<Observation> seen = new HashSet<>();
    private final List<Observation> observations = new ArrayList<>();
    boolean partial;

    Facts(Category category, int limit, List<Path> paths) {
        this.category = category;
        this.limit = limit;
        for (int index = 0; index < paths.size(); index++) {
            sources.put(paths.get(index).toAbsolutePath().normalize(), index);
        }
    }

    Integer source(Path path) {
        return sources.get(path.toAbsolutePath().normalize());
    }

    void add(Range occurrence, Range enclosing, Anchor target) {
        if (occurrence == null) {
            return;
        }
        if (enclosing != null &&
            (enclosing.start() > occurrence.start() || enclosing.end() < occurrence.end())) {
            enclosing = null;
        }
        var fact = new Observation(occurrence, enclosing, target);
        if (seen.contains(fact)) {
            return;
        }
        if (observations.size() == limit) {
            partial = true;
            return;
        }
        seen.add(fact);
        observations.add(fact);
    }

    List<Observation> observations() {
        observations.sort(Comparator
            .comparingInt((Observation fact) -> fact.occurrence().start())
            .thenComparingInt(fact -> fact.occurrence().end()));
        return observations;
    }
}
