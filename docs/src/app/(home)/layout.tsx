import { GithubLogoIcon } from "@phosphor-icons/react/dist/ssr";
import { HomeLayout } from "fumadocs-ui/layouts/home";

import { baseOptions } from "@/lib/layout.shared";

export default function Layout({ children }: LayoutProps<"/">) {
  return (
    <HomeLayout {...baseOptions()}>
      {children}
      <footer className="border-t border-border">
        <div className="mx-auto flex w-full max-w-190 items-center justify-between gap-6 px-7 py-6 font-mono text-[11px] tracking-[0.08em] text-muted-foreground">
          <a
            href="https://github.com/volarized/rift"
            target="_blank"
            rel="noreferrer noopener"
            className="flex items-center gap-2 transition-colors hover:text-foreground"
          >
            <GithubLogoIcon size={14} weight="light" />
            <span className="underline underline-offset-2">rift</span>
          </a>
          <span>crafted with precision in Berlin</span>
        </div>
      </footer>
    </HomeLayout>
  );
}
