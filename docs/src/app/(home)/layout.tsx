import { HomeLayout } from 'fumadocs-ui/layouts/home';

import { baseOptions } from '@/lib/layout.shared';

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <HomeLayout {...baseOptions()}>
      {children}
      <footer className="border-t border-border">
        <div className="mx-auto flex w-full max-w-190 justify-between gap-6 px-7 py-6 font-mono text-[11px] tracking-[0.08em] text-muted-foreground">
          <span>greif</span>
          <a
            href="https://volar.sh"
            className="border-b border-border transition-colors hover:border-foreground hover:text-foreground"
          >
            volar.sh
          </a>
        </div>
      </footer>
    </HomeLayout>
  );
}
