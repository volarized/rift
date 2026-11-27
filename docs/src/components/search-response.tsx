import { createMarkdownRenderer } from "fumadocs-core/content/md";
import { remarkGfm } from "fumadocs-core/mdx-plugins/remark-gfm";

const { MarkdownServer } = createMarkdownRenderer({ remarkPlugins: [remarkGfm] });

export function SearchResponse({ source }: { source: string }) {
  return (
    <div className="min-w-0 space-y-3 p-4">
      <MarkdownServer
        components={{
          p: ({ children }) => <p className="text-sm leading-relaxed">{children}</p>,
          table: ({ children }) => (
            <section
              aria-label="Search results"
              className="max-w-full overflow-x-auto border border-fd-border focus-visible:outline-1 focus-visible:outline-fd-primary"
            >
              <table className="w-full min-w-max border-collapse text-left font-mono text-[13px] leading-relaxed">
                {children}
              </table>
            </section>
          ),
          thead: ({ children }) => (
            <thead className="border-b border-fd-border bg-fd-secondary">{children}</thead>
          ),
          th: ({ children }) => (
            <th className="px-4 py-2.5 font-semibold" scope="col">
              {children}
            </th>
          ),
          tbody: ({ children }) => <tbody className="divide-y divide-fd-border">{children}</tbody>,
          td: ({ children }) => (
            <td className="px-4 py-2.5 align-top">
              <span className="block max-w-[30rem] break-words whitespace-pre-wrap">
                {children}
              </span>
            </td>
          ),
        }}
      >
        {source}
      </MarkdownServer>
    </div>
  );
}
