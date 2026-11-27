import com.google.gson.FieldNamingPolicy;
import com.google.gson.Gson;
import com.google.gson.GsonBuilder;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.Strictness;
import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.OutputStreamWriter;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.regex.Pattern;

final class CollectorProtocol {
    private static final Gson JSON = new GsonBuilder()
        .setStrictness(Strictness.STRICT)
        .disableJdkUnsafe()
        .disableHtmlEscaping()
        .setFieldNamingPolicy(FieldNamingPolicy.LOWER_CASE_WITH_UNDERSCORES)
        .serializeNulls()
        .create();

    private static final int MESSAGE_BYTES_MAX = 8 << 20;
    private static final int HEADER_BYTES_MAX = 4 << 10;
    private static final Pattern CONTENT_LENGTH = Pattern.compile("(?i)Content-Length: ([0-9]{1,8})\r\n");
    private static final String JSON_RPC = "2.0";
    private final InputStream input;
    private final OutputStream output;
    private long supportId;

    record Source(String unit, String filename, String sha256, String text) {}
    record Extraction(int version, Facts.Category category, Integer observationsMax, Source source) {}
    record Request(String jsonrpc, JsonElement id, String method, Extraction params) {}
    private record Support(String unit) {}
    private record SupportRequest(String jsonrpc, String id, String method, Support params) {}
    private record SupportResponse(String jsonrpc, JsonElement id, Source result) {}
    private record Response(String jsonrpc, JsonElement id, JvmCollector.Batch result) {}

    CollectorProtocol(InputStream input, OutputStream output) {
        this.input = new BufferedInputStream(input);
        this.output = new BufferedOutputStream(output);
    }

    Request next() throws IOException {
        JsonObject message = read();
        if (message == null) {
            return null;
        }
        Request request = JSON.fromJson(message, Request.class);
        if (!JSON_RPC.equals(request.jsonrpc()) || !"extractor/extract".equals(request.method())
            || request.id() == null || !request.id().isJsonPrimitive()
            || !request.id().getAsJsonPrimitive().isNumber() || request.params() == null) {
            throw new IOException("invalid extraction request");
        }
        return request;
    }

    Source supportingSource(String unit) throws IOException {
        supportId = Math.incrementExact(supportId);
        String id = "source-" + supportId;
        send(new SupportRequest(JSON_RPC, id, "facts/request", new Support(unit)));
        JsonObject message = read();
        if (message == null || !message.has("result") || message.has("error")) {
            throw new IOException("invalid supporting source response");
        }
        var response = JSON.fromJson(message, SupportResponse.class);
        if (!JSON_RPC.equals(response.jsonrpc()) || response.id() == null || !response.id().isJsonPrimitive()
            || !response.id().getAsJsonPrimitive().isString() || !id.equals(response.id().getAsString())) {
            throw new IOException("invalid supporting source response");
        }
        Source source = response.result();
        if (source != null && !unit.equals(source.unit())) {
            throw new IOException("supporting source unit does not match the request");
        }
        return source;
    }

    void reply(JsonElement id, JvmCollector.Batch batch) throws IOException {
        send(new Response(JSON_RPC, id, batch));
    }

    private JsonObject read() throws IOException {
        Integer length = readLength();
        if (length == null) {
            return null;
        }
        byte[] bytes = input.readNBytes(length);
        if (bytes.length != length) {
            throw new EOFException("incomplete frame");
        }
        var text = StandardCharsets.UTF_8.newDecoder().decode(ByteBuffer.wrap(bytes));
        JsonObject message = JSON.fromJson(text.toString(), JsonObject.class);
        if (message == null) {
            throw new IOException("invalid JSON-RPC message");
        }
        return message;
    }

    private Integer readLength() throws IOException {
        StringBuilder line = new StringBuilder();
        for (int size = 0; size < HEADER_BYTES_MAX; size++) {
            int next = input.read();
            if (next == -1 && size == 0) {
                return null;
            }
            if (next == -1) {
                throw new EOFException("incomplete frame header");
            }
            line.append((char) next);
            if (next == '\n') {
                var header = CONTENT_LENGTH.matcher(line);
                if (!header.matches() || input.read() != '\r' || input.read() != '\n') {
                    throw new IOException("invalid Content-Length header");
                }
                int length = Integer.parseInt(header.group(1));
                if (length == 0 || length > MESSAGE_BYTES_MAX) {
                    throw new IOException("invalid frame size");
                }
                return length;
            }
        }
        throw new IOException("header exceeds frame bound");
    }

    private void send(Object message) throws IOException {
        var body = new FrameBody();
        try (var writer = new OutputStreamWriter(body, StandardCharsets.UTF_8)) {
            JSON.toJson(message, writer);
        }
        output.write(("Content-Length: " + body.size() + "\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
        body.writeTo(output);
        output.flush();
    }

    private static final class FrameBody extends ByteArrayOutputStream {
        @Override
        public synchronized void write(int value) {
            limit(1);
            super.write(value);
        }

        @Override
        public synchronized void write(byte[] bytes, int offset, int size) {
            limit(size);
            super.write(bytes, offset, size);
        }

        private void limit(int size) {
            if (size > MESSAGE_BYTES_MAX - count) {
                throw new IllegalArgumentException("response exceeds frame bound");
            }
        }
    }
}
