// Collect authored JS, JSX, TS and TSX calls with an injected TypeScript engine.

import { once } from "node:events";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const FRAME_BYTES_MAX = 8 << 20;
const HEADER_BYTES_MAX = 4 << 10;
const PROGRAM_OPTIONS = {
  allowJs: true,
  checkJs: true,
  noLib: true,
  noResolve: true,
  types: [],
};

function offsets(text) {
  const bytes = new Uint32Array(text.length + 1);
  let unit = 0;
  let byte = 0;
  for (const point of text) {
    unit += point.length;
    byte += Buffer.byteLength(point);
    bytes[unit] = byte;
  }
  return bytes;
}

function localChecker(ts, source) {
  const host = {
    getCanonicalFileName: name => name,
    getCurrentDirectory: () => "",
    getDefaultLibFileName: () => "",
    getNewLine: () => "\n",
    useCaseSensitiveFileNames: () => true,
    writeFile: () => { throw new Error("collection does not emit source"); },
    getSourceFile: name => name === source.fileName ? source : undefined,
    fileExists: name => name === source.fileName,
    readFile: name => name === source.fileName ? source.text : undefined,
  };
  return ts.createProgram([source.fileName], PROGRAM_OPTIONS, host).getTypeChecker();
}

/**
 * Collect calls from admitted source. A caller can supply the AST and checker from
 * an existing program when they describe the same document.
 */
export function collect(ts, request, admitted) {
  if (request.category !== "calls") {
    return { coverage: "unsupported", observations: [] };
  }
  const { filename, text, unit, sha256 } = request.source;
  const source = admitted?.source ?? ts.createSourceFile(filename, text, ts.ScriptTarget.ES2022, true);
  if (source.text !== text || source.fileName !== filename) {
    throw new Error("AST does not describe the admitted source");
  }
  if (!Array.isArray(source.parseDiagnostics) || source.parseDiagnostics.length) {
    return { coverage: "partial", observations: [] };
  }
  const checker = admitted?.checker ?? localChecker(ts, source);
  const bytes = offsets(text);
  const span = node => ({ start: bytes[node.getStart(source)], end: bytes[node.getEnd()] });
  const observations = [];
  const pending = [[source, null]];
  while (pending.length) {
    const [node, owner] = pending.pop();
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      if (observations.length === request.observations_max) {
        return { coverage: "partial", observations };
      }
      const targetNode = checker.getResolvedSignature(node)?.declaration;
      const target = targetNode?.getSourceFile() === source ? { unit, source_sha256: sha256, range: span(targetNode) } : null;
      observations.push({
        id: String(observations.length),
        occurrence: span(node),
        enclosing: owner,
        target,
        evidence: target === null ? "syntax" : "candidate",
      });
    }
    const childOwner = ts.isFunctionLike(node) || ts.isClassLike(node) ? span(node) : owner;
    const children = [];
    node.forEachChild(child => { children.push(child); });
    for (const child of children.reverse()) pending.push([child, childOwner]);
  }
  return { coverage: "complete", observations };
}

class Frames {
  constructor(input) {
    this.input = input[Symbol.asyncIterator]();
    this.buffer = Buffer.alloc(0);
  }

  async fill() {
    const next = await this.input.next();
    if (next.done) return false;
    this.buffer = next.value;
    return true;
  }

  async read() {
    const header = [];
    while (header.length < HEADER_BYTES_MAX) {
      if (!this.buffer.length && !await this.fill()) {
        if (!header.length) return null;
        throw new Error("incomplete header");
      }
      const byte = this.buffer[0];
      this.buffer = this.buffer.subarray(1);
      header.push(byte);
      if (byte === 10 && header.at(-2) === 13 && header.at(-3) === 10 && header.at(-4) === 13) {
        const match = /^Content-Length: ([0-9]{1,8})\r\n\r\n$/i.exec(Buffer.from(header).toString("ascii"));
        const length = Number(match?.[1]);
        if (!(length > 0 && length <= FRAME_BYTES_MAX)) throw new Error("invalid frame size");
        return JSON.parse(await this.body(length));
      }
    }
    throw new Error("header exceeds byte bound");
  }

  async body(length) {
    const output = Buffer.alloc(length);
    let copied = 0;
    while (copied < length) {
      if (!this.buffer.length && !await this.fill()) throw new Error("incomplete frame");
      const count = this.buffer.copy(output, copied);
      copied += count;
      this.buffer = this.buffer.subarray(count);
    }
    return output.toString("utf8");
  }
}

async function send(output, message) {
  const data = Buffer.from(JSON.stringify(message));
  if (data.length > FRAME_BYTES_MAX) throw new Error("response exceeds frame bound");
  const frame = Buffer.concat([Buffer.from(`Content-Length: ${data.length}\r\n\r\n`), data]);
  if (!output.write(frame)) await once(output, "drain");
}

/** Run a direct collector over stdio. Await each supporting source request. */
export async function serve(extract, input = process.stdin, output = process.stdout) {
  const frames = new Frames(input);
  let supportId = 0;
  const readSource = async unit => {
    const id = `source-${++supportId}`;
    await send(output, { jsonrpc: "2.0", id, method: "facts/request", params: { unit } });
    const response = await frames.read();
    if (response?.jsonrpc !== "2.0" || response.id !== id || !Object.hasOwn(response, "result")) {
      throw new Error("invalid supporting source response");
    }
    return response.result;
  };
  for (let request; (request = await frames.read()) !== null;) {
    if (request.jsonrpc !== "2.0" || request.method !== "extractor/extract" || request.params?.version !== 1) {
      throw new Error("invalid extraction request or contract version");
    }
    const result = await extract(request.params, readSource);
    await send(output, { jsonrpc: "2.0", id: request.id, result });
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  const [engine, custom, ...rest] = process.argv.slice(2);
  if (!engine || rest.length) throw new Error("usage: javascript.mjs TYPESCRIPT_ENTRY [COLLECTOR_MODULE]");
  const ts = (await import(pathToFileURL(resolve(engine)))).default;
  if (ts.version !== "5.9.3") throw new Error("expected TypeScript 5.9.3");
  const extract = custom
    ? (await import(pathToFileURL(resolve(custom)))).collect
    : (_engine, request) => collect(_engine, request);
  await serve((request, readSource) => extract(ts, request, readSource));
}
