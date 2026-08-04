import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { Prose } from "@/components/protocol/prose";
import { Shape } from "@/components/protocol/shape";
import { defs } from "@/lib/protocol";
import { ADAPTER_HELLO, adapterOperations, referenceTypes } from "@/lib/protocol-surface";

/**
 * The adapter wire: one operation at a time, request beside response.
 *
 * Operations are paired by the `op` const both frames carry, not by name, so
 * this catalogue cannot drift from the schema. Streaming operations answer with
 * more than one response frame — a chunk and an end — and both are shown,
 * because "where does the stream stop" is the first question anyone asks.
 */
export function AdapterReference(): ReactNode {
  const hello = defs[ADAPTER_HELLO];

  return (
    <>
      <h2 id="handshake" className="scroll-m-20">
        Handshake
      </h2>
      <p>
        Before anything else, the adapter says what it can do. Rift does not probe, guess, or
        feature-test — it reads this frame once and holds every adapter to exactly what it claimed
        here.
      </p>
      {hello?.description ? (
        <p>
          <Prose text={hello.description} />
        </p>
      ) : null}
      <Shape label="Frame" name={ADAPTER_HELLO} />

      <h2 id="operations" className="scroll-m-20">
        Operations
      </h2>
      <p>
        Every frame carries a fully qualified <code>type</code>, so a recorded stream can be read
        back without knowing what was asked — which is what lets a captured session double as a
        conformance fixture. The <code>op</code> beside it groups frames that belong to the same
        call.
      </p>

      {adapterOperations.map((operation) => (
        <section key={operation.op}>
          <h3 id={`op-${operation.op}`} className="scroll-m-20 font-mono">
            {operation.op}
          </h3>
          {operation.description ? (
            <p>
              <Prose text={operation.description} />
            </p>
          ) : null}
          <Shape label="Request" name={operation.request.name} />
          {operation.responses.map((response) => (
            <Shape
              key={response.name}
              label={
                operation.responses.length > 1
                  ? (response.tag.split(".").pop() ?? "Response")
                  : "Response"
              }
              name={response.name}
            />
          ))}
        </section>
      ))}

      <h2 id="type-reference" className="scroll-m-20">
        Type reference
      </h2>
      <p>
        The remaining {referenceTypes.adapter.length} types this document defines, in schema order.
        Frames the operations above already showed are anchored there instead.
      </p>
      {referenceTypes.adapter.map((name) => (
        <Definition key={name} name={name} />
      ))}
    </>
  );
}
