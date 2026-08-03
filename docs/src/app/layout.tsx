import '@/app/global.css';
import type { Metadata } from 'next';
import { RootProvider } from 'fumadocs-ui/provider';
import { Inter, Geist_Mono } from 'next/font/google';
import { cn } from '@/lib/utils';

// Two families, per the design system: Geist Mono for display, section
// labels and code; Inter 300 for lead and body copy.
const geistMono = Geist_Mono({
  subsets: ['latin'],
  weight: ['400', '500'],
  variable: '--font-geist-mono',
});

const inter = Inter({
  subsets: ['latin'],
  weight: ['300', '400', '500'],
  variable: '--font-inter',
});

export const metadata: Metadata = {
  title: {
    default: 'greif — agentic development toolkit',
    template: '%s — greif',
  },
  description:
    'greif is an MCP assistant that provides capabilities for agentic-driven, typesafe development. It provides tools and resources to read, search, discover and edit codebases.',
  icons: { icon: '/logo.svg' },
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html
      lang="en"
      className={cn(inter.variable, geistMono.variable)}
      suppressHydrationWarning
    >
      <body className="flex flex-col min-h-screen">
        <RootProvider>{children}</RootProvider>
      </body>
    </html>
  );
}
