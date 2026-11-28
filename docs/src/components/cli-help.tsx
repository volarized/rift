import { readFileSync } from "node:fs";
import { join } from "node:path";
import { CodeBlock, Pre } from "fumadocs-ui/components/codeblock";

/** Renders the CLI help transcript exported by the docs build. */
export function CliHelp() {
  const transcript = readFileSync(join(process.cwd(), "public/cli-help.txt"), "utf8").trimEnd();
  return (
    <CodeBlock>
      <Pre>
        <code>{transcript}</code>
      </Pre>
    </CodeBlock>
  );
}
