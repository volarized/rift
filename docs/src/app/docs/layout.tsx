import { DocsLayout } from "fumadocs-ui/layouts/docs";
import { VersionBanner } from "@/components/version-banner";
import { VersionSwitch } from "@/components/version-switch";
import { baseOptions } from "@/lib/layout.shared";
import { source } from "@/lib/source";

export default function Layout({ children }: LayoutProps<"/docs">) {
  return (
    <>
      <VersionBanner />
      <DocsLayout
        tree={source.pageTree}
        {...baseOptions()}
        sidebar={{ tabs: false, footer: <VersionSwitch /> }}
      >
        {children}
      </DocsLayout>
    </>
  );
}
