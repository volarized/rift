import { ServerCodeBlock } from "fumadocs-ui/components/codeblock.rsc";
import { Files, Folder } from "fumadocs-ui/components/files";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "fumadocs-ui/components/tabs";
import { FileIcon } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { exampleProjectFiles } from "@/lib/example-project";

function FileButton({ path }: { path: string }) {
  return (
    <TabsTrigger
      value={path}
      className="w-full justify-start rounded-none border-0 px-2 py-1.5 font-mono font-normal hover:bg-fd-accent focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring data-[state=active]:bg-fd-accent data-[state=active]:text-fd-accent-foreground"
    >
      <FileIcon aria-hidden="true" />
      {path.split("/").at(-1)}
    </TabsTrigger>
  );
}

export async function ExampleProject() {
  const previews = await Promise.all(
    exampleProjectFiles.map(async (file) => ({
      path: file.path,
      content: await ServerCodeBlock({
        code: file.code,
        lang: file.language,
        codeblock: {
          title: `${file.path}${file.excerpt ? " (excerpt)" : ""}`,
          className:
            "m-0 rounded-none border-0 shadow-none [&_pre]:w-full [&_pre]:break-words [&_pre]:whitespace-pre-wrap",
          viewportProps: {
            "aria-label": `${file.path} source`,
            className: "h-96 max-h-96",
          },
        },
      }),
    })),
  );

  return (
    <Tabs
      defaultValue="src/config.rs"
      orientation="vertical"
      className="not-prose @container my-6 rounded-none bg-fd-card"
      aria-label="Example project"
    >
      <div className="grid @min-[40rem]:grid-cols-[11rem_minmax(0,1fr)]">
        <div className="min-w-0 border-b @min-[40rem]:border-r @min-[40rem]:border-b-0">
          <div className="flex h-10 items-center justify-between border-b px-3 text-sm">
            <span>Files</span>
            <Badge variant="outline" className="text-muted-foreground">
              Rust
            </Badge>
          </div>
          <Files className="rounded-none border-0 bg-transparent p-2 font-mono">
            <TabsList
              aria-label="Example project files"
              className="flex-col items-stretch gap-0 overflow-visible p-0"
            >
              {exampleProjectFiles
                .filter((file) => !file.path.startsWith("src/"))
                .map((file) => (
                  <FileButton key={file.path} path={file.path} />
                ))}
              <Folder name="src" defaultOpen>
                {exampleProjectFiles
                  .filter((file) => file.path.startsWith("src/"))
                  .map((file) => (
                    <FileButton key={file.path} path={file.path} />
                  ))}
              </Folder>
            </TabsList>
          </Files>
        </div>
        <div className="min-w-0">
          {previews.map((preview) => (
            <TabsContent
              key={preview.path}
              value={preview.path}
              className="rounded-none p-0 [&>figure:only-child]:m-0"
            >
              {preview.content}
            </TabsContent>
          ))}
        </div>
      </div>
    </Tabs>
  );
}
