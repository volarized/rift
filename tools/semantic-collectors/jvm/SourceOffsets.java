import java.io.IOException;

final class SourceOffsets {
    private final int[] offsets;
    private final int units;

    SourceOffsets(String source, boolean normalizeLines) throws IOException {
        offsets = new int[source.length() + 1];
        int length = 0;
        int bytes = 0;
        for (int index = 0; index < source.length();) {
            int point = source.codePointAt(index);
            if (normalizeLines && point == '\r' && index + 1 < source.length() && source.charAt(index + 1) == '\n') {
                index++;
                bytes++;
                continue;
            }
            if (point >= Character.MIN_SURROGATE && point <= Character.MAX_SURROGATE) {
                throw new IOException("source contains an unpaired UTF-16 surrogate");
            }
            int width = Character.charCount(point);
            index += width;
            if (width == 2) {
                offsets[++length] = -1;
            }
            bytes += point < 0x80 ? 1 : point < 0x800 ? 2 : point < 0x10000 ? 3 : 4;
            offsets[++length] = bytes;
        }
        units = length;
    }

    int bytes() {
        return offsets[units];
    }

    Facts.Range bytes(Facts.Range range) throws IOException {
        if (range == null) {
            return null;
        }
        if (range.end() > units || offsets[range.start()] < 0 || offsets[range.end()] < 0) {
            throw new IOException("compiler returned an invalid source range");
        }
        return new Facts.Range(offsets[range.start()], offsets[range.end()]);
    }
}
