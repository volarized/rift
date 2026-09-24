import { GithubLogoIcon } from "@phosphor-icons/react/dist/ssr";
import { HomeLayout } from "fumadocs-ui/layouts/home";

import { baseOptions } from "@/lib/layout.shared";

export default function Layout({ children }: LayoutProps<"/">) {
  return (
    <HomeLayout {...baseOptions()}>
      {children}
      <footer className="border-t border-border">
        <div className="mx-auto grid w-full max-w-190 grid-cols-2 items-center gap-3 px-6 py-6 font-mono text-[11px] tracking-[0.08em] text-muted-foreground sm:grid-cols-[1fr_auto_1fr] sm:gap-6 sm:px-7">
          <a
            href="https://github.com/volarized/rift"
            target="_blank"
            rel="noreferrer noopener"
            className="col-start-1 row-start-2 flex items-center gap-2 justify-self-start transition-colors hover:text-foreground sm:row-start-1"
          >
            <GithubLogoIcon size={14} weight="light" />
            <span className="underline underline-offset-2">rift</span>
          </a>
          <span className="col-span-2 row-start-1 text-center sm:col-span-1 sm:col-start-2">
            crafted with precision in Berlin
          </span>
          <a
            href="mailto:contact@volar.sh"
            className="col-start-2 row-start-2 justify-self-end underline underline-offset-2 transition-colors hover:text-foreground sm:col-start-3 sm:row-start-1"
          >
            Contact us
          </a>
        </div>
      </footer>
    </HomeLayout>
  );
}
