import { DocsLayout } from "fumadocs-ui/layouts/docs";
import { baseOptions, docsLinks } from "@/lib/layout.shared";
import { source } from "@/lib/source";

export default function Layout({ children }: LayoutProps<"/docs">) {
  return (
    <DocsLayout
      tree={source.pageTree}
      {...baseOptions()}
      links={docsLinks}
      sidebar={{ tabs: false }}
    >
      {children}
    </DocsLayout>
  );
}
