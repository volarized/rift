import type { Metadata } from "next";
import Link from "next/link";

import { Command } from "@/components/command";
import { GLOBAL_MCP_ENDPOINT, GLOBAL_REST_ENDPOINT } from "@/components/global/constants";
import { GlobalConsole } from "@/components/global/global-console";

export const metadata: Metadata = {
  title: "Global",
  description:
    "Console for the rift global index: send requests to the global API from your browser.",
};

export default function GlobalPage() {
  return (
    // The home layout width, not the prose column: the console needs the room.
    <main className="mx-auto w-full max-w-(--fd-layout-width) px-4 py-16 md:px-6">
      <GlobalConsole title="Global">
        <p className="text-muted-foreground">
          The global index holds facts about top-used OSS dependencies. This page sends each request
          from your browser straight to the global API.
        </p>
        {/* One outlined region, two rows: the second box sits on the first one's rule.
            The MCP prompt is padded so both URLs start in the same column. */}
        <div className="grid">
          <Command prompt="REST" copyLabel="Copy the REST endpoint">
            {GLOBAL_REST_ENDPOINT}
          </Command>
          <Command prompt="MCP " copyLabel="Copy the MCP endpoint" className="-mt-px">
            {GLOBAL_MCP_ENDPOINT}
          </Command>
        </div>
        <p className="text-sm text-muted-foreground">
          The API accepts anonymous requests. It records daily request counts; a{" "}
          <code className="font-mono text-[13px]">DNT: 1</code> header opts out - see{" "}
          <Link className="underline underline-offset-2 hover:text-foreground" href="/docs/privacy">
            Privacy
          </Link>
          .
        </p>
      </GlobalConsole>
    </main>
  );
}
