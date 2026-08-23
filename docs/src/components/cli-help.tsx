import { readFileSync } from "node:fs";
import { join } from "node:path";
import { CodeBlock, Pre } from "fumadocs-ui/components/codeblock";

/**
 * Renders the generated CLI help transcript from `public/cli-help.txt`.
 * `just generate` writes the file from the built binary; the page never
 * hand-types command output.
 */
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
