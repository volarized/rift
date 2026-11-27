# Semantic collection

`rift-collect` runs a collector over an admitted inventory of UTF-8 sources. It emits one JSON line per selected source and category. Each line names the producer, its implementation and context revisions, the canonical source unit, the source digest, coverage, and the original observations.

The Python collector uses the standard library AST. It records explicit calls, including repeated and module-level occurrences. The JavaScript collector uses TypeScript 5.9.3 for JavaScript, JSX, TypeScript and TSX. It also records same-source signature declarations as candidates. Both parse target source without executing it and report unsupported coverage for categories other than calls. The JVM compiler collector records calls, references and imports in Java and Kotlin.

## Run and rerun

Build the collector runner with `cargo build --locked -p rift-collect`. Create a manifest with a stable producer ID and lowercase SHA-256 digests for its implementation and context. Use the implementation digest for the collector and adapter code. Use the context digest for engine and runtime versions, compiler options, and other inputs outside the admitted inventory. Change the corresponding digest when one of those inputs changes.

For a Python run from the repository root:

```sh
printf 'def greet(): pass\ngreet()\n' > example.py
revision=$(shasum -a 256 tools/semantic-collectors/python.py | cut -d ' ' -f 1)
context=$(python3 --version | shasum -a 256 | cut -d ' ' -f 1)
cat > sources.json <<JSON
{
  "producer": "python.calls",
  "revision": "$revision",
  "context": "$context",
  "category": "calls",
  "sources": [{
    "unit": "rift://source/project/example.py",
    "path": "example.py",
    "filename": "example.py"
  }]
}
JSON
cargo run --locked -q -p rift-collect -- collect sources.json checkpoints.sqlite -- \
  python3 -B -S tools/semantic-collectors/python.py
```

For JavaScript, use a JavaScript or TypeScript source and a distinct producer ID. Pass `node tools/semantic-collectors/javascript.mjs /path/to/typescript/lib/typescript.js` after `--`. Include the engine and runtime in the context digest.

The manifest resolves `path` relative to its own directory. The collector uses `filename` to select parser behavior and `unit` to identify the source. Set `"selected": false` on a dependency that collectors can read without collecting its own batch.

Repeat the command with the same database. The runner reuses a checkpoint when the producer, context, category, selected source and supporting source witnesses match. A read of a missing unit is also a witness. Adding that unit invalidates the dependent batch. Removing a unit from the selection removes it from subsequent output. Complete batches commit independently. A restart after failure reuses them. A partial batch remains due, and the CLI exits with status 2 after returning its output. Other failures exit with status 1.

Use a separate database for each collection. A new database has a 1 GiB page limit. The runner accepts up to 20,000 sources, 128 MiB of total source text and 1 MiB per source. It admits up to 16,384 observations and 512 supporting units per batch. The stdio runner gives each request 30 seconds. A complete batch establishes coverage of the selected source and category. A candidate does not establish runtime dispatch.

## Run the Java and Kotlin syntax and LSP collectors

Use a manifest with category `calls`, `references`, or `imports`. List Java or Kotlin source
files with their own extensions in `filename`. The syntax collector records authored occurrences
in Java, Kotlin source, and Kotlin scripts.

```sh
rift-collect static sources.json syntax.sqlite
rift-collect lsp java sources.json java-lsp.sqlite
rift-collect lsp kotlin sources.json kotlin-lsp.sqlite
```

The LSP collector resolves those occurrences through the selected language server. Keep the manifest
in the project root and admit the source files that can supply target declarations. Set
`"selected": false` to admit a target without collecting its own batch. A definition URI without a
matching admitted file stays unresolved. Include the build files, toolchain, server and classpath
inputs outside the inventory in the context digest. The runner adds the paths and digests of all
admitted files to the LSP context.

By default the Java session starts `jdtls` with `-data` under `.rift/jdtls`. The Kotlin session
starts `kotlin-lsp --stdio` with `--system-path` under `.rift/kotlin-lsp`. Put the selected server
on `PATH`. Pass `-- PROGRAM [ARGS...]` to select a different command. The server loads the project
and its dependencies. See [LSP recommendations](../../docs/content/docs/developer/configuration.mdx#lsp-recommendations)
for Maven and Gradle detection and server setup.

## Run the JVM compiler collector

The Java collector uses the JDK compiler tree API. The Kotlin collector uses K2 from Kotlin
2.5.0-RC2. Both resolve names and overloads against the admitted source and the compiler
classpath. A target has an anchor only when the compiler reports a declaration range in an
admitted source.

Set `JAVA_HOME` to a JDK 25 installation and `KOTLIN_HOME` to the extracted Kotlin 2.5.0-RC2
distribution. The collector uses the PSI interfaces in this compiler version. Verify
`kotlin-compiler-2.5.0-RC2.zip` against SHA-256
`081346eeb7002606d436de6e407252eb6d9a6b58ac497202eef9c16d51bf17b9`.
[Kotlin 2.5.0-RC2](https://github.com/JetBrains/kotlin/releases/tag/v2.5.0-RC2).

Set `GSON_JAR` to the Gson 2.13.2 JAR (`com.google.code.gson:gson:2.13.2`).
[Gson 2.13.2](https://github.com/google/gson/releases/tag/gson-parent-2.13.2).

Use the standard JAR file system when building and running the Kotlin collector. The Kotlin
worker requires the JVM to deny legacy unsafe memory access and exclude `jdk.unsupported`.

Run these commands from the repository root to build the collector.

```sh
build=$(mktemp -d "${TMPDIR:-/tmp}/rift-jvm.XXXXXXXX")
mkdir "$build/classes"
sources=tools/semantic-collectors/jvm
jvm_modules=--limit-modules=java.se,jdk.compiler,jdk.zipfs,jdk.management
jvm_memory_access=--sun-misc-unsafe-memory-access=deny
"$JAVA_HOME/bin/javac" -proc:none -cp "$GSON_JAR" -d "$build/classes" "$sources"/*.java
"$JAVA_HOME/bin/java" "$jvm_modules" "$jvm_memory_access" -Xmx768m \
  -cp "$KOTLIN_HOME/lib/*" org.jetbrains.kotlin.cli.jvm.K2JVMCompiler \
  -kotlin-home "$KOTLIN_HOME" -Xuse-fast-jar-file-system=false -Xdisable-default-scripting-plugin \
  -classpath "$KOTLIN_HOME/lib/kotlin-compiler.jar:$build/classes" \
  -d "$build/classes" "$sources/KotlinFacts.kt"
"$JAVA_HOME/bin/jar" --create --file "$build/jvm-facts.jar" \
  -C "$build/classes" . -C "$sources" META-INF
```

Pass the JVM command to `rift-collect collect`. The manifest category can be `calls`,
`references`, or `imports`.

```sh
rift-collect collect sources.json checkpoints.sqlite -- \
  "$JAVA_HOME/bin/java" "$jvm_modules" "$jvm_memory_access" -Xmx768m \
  -cp "$build/jvm-facts.jar:$GSON_JAR:$KOTLIN_HOME/lib/*" \
  JvmCollector --kotlin-home "$KOTLIN_HOME"
```

The runner starts one JVM on the first cache miss and uses it for later batches. For a Java-only
build, compile the Java sources and package the classes. Launch `JvmCollector` with
`"$build/jvm-facts.jar:$GSON_JAR"` as the JVM classpath. The Kotlin startup arguments apply to
workers configured with `--kotlin-home`.

Use `--source-unit UNIT` for each supporting source in the manifest. For a build classpath, pass
one `--classpath PATH` per classes directory or JAR. Include the JVM collector files in the
implementation digest. Include the Gson JAR, JDK and Kotlin versions, classpath contents,
supporting unit names and compiler options in the context digest.

For a trusted Gradle JVM build, add `--gradle-root /path/to/project` before `--`. The runner uses
the project's wrapper or `gradle` on `PATH` and gives the context export three minutes. Set
`JAVA_HOME` for Gradle. You can use a different JDK in the worker command after `--`.

Gradle matches admitted files to their compile tasks and supplies source set dependencies, Kotlin
opt-ins, language and JVM versions, and friend paths. Build the project first if a source depends
on generated classes, including KSP output. The collector reads existing class outputs. Gradle
dependency resolution can use the project's configured repositories during export. A selected
Java or Kotlin file must belong to one compile task.

Admitted Java sources from project dependencies can supply declaration anchors when their class
output or main JAR is on the compile classpath. An explicit Kotlin language version
must be 2.2 or later. Gradle builds that leave the language version unset use the compiler default.

The runner adds the Gradle model and classpath content digests to its checkpoint context. Keep the
worker, compiler and runtime versions and any manually supplied inputs in the manifest digests.

For JDK source from `$JAVA_HOME/lib/src.zip`, pass `--java-module java.base` and one `--java-module-source UNIT` per admitted file in that module. For Kotlin standard library source from the matching source JAR, pass `--kotlin-stdlib` and the admitted unit with `--source-unit UNIT`. The option lets the compiler collect declarations in the `kotlin` package against its matching standard library.

The compiler reads only the selected and supporting source units, plus compiler and classpath
inputs. Java annotation processing and Kotlin scripting are disabled. Set the JVM option
`-Djava.io.tmpdir=PATH` before `JvmCollector` to choose where the collector stages sources and
compiler output. A forced stop can leave the current request's temporary directory behind.

The collector reports partial coverage if a compiler reports an error or a source or observation
bound interrupts collection. Kotlin scripts (`.kts`) use the static analyzer.

## Use a collector from another host

Implement `rift_collect::Collector` for an in-process Rust provider. The trait takes an admitted `CollectionInput` and returns a `Batch`. The provider can read supporting units through `CollectionInput::read`. Disable the default `runtime` feature to use the model without process I/O or SQLite. Use `rift_collect::CollectionCache` when local checkpoints are required.

Python modules can export `collect(request, read_source)` and be passed after `python.py`. JavaScript modules can export `collect(ts, request, readSource)` and be passed after the TypeScript entry point. Each call to `readSource` must finish before another starts. A module may also call the built-in collector directly. JavaScript can supply an AST and checker from its own program for the exact admitted source.

`rift-collect schema` emits the request and result schemas. The stdio adapter sends `extractor/extract` using Content-Length framed JSON-RPC. A collector can issue `facts/request` with a canonical `unit` while extraction is pending. The runner returns the admitted source or `null`. A physical target must include the digest and byte range of the inspected source. The runner validates occurrence, enclosing, and target coordinates before checkpointing. Collector code runs with the privileges of the caller. Use stdout for protocol messages and stderr for diagnostics.
