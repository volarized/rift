import { DocsLayout } from "fumadocs-ui/layouts/docs";
import { VersionBanner } from "@/components/version-banner";
import { baseOptions } from "@/lib/layout.shared";
import { source } from "@/lib/source";

export default function Layout({ children }: LayoutProps<"/docs">) {
  return (
    <>
      <VersionBanner />
      <DocsLayout tree={source.pageTree} {...baseOptions()}>
        {children}
      </DocsLayout>
    </>
  );
}
