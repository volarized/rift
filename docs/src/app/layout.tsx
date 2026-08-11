import "@/app/global.css";
import { RootProvider } from "fumadocs-ui/provider";
import type { Metadata } from "next";
import { Geist_Mono, Inter } from "next/font/google";
import { cn } from "@/lib/utils";

// Two families, per the design system: Geist Mono for display, section
// labels and code; Inter 300 for lead and body copy.
const geistMono = Geist_Mono({
  subsets: ["latin"],
  weight: ["400", "500"],
  variable: "--font-geist-mono",
});

const inter = Inter({
  subsets: ["latin"],
  weight: ["300", "400", "500"],
  variable: "--font-inter",
});

export const metadata: Metadata = {
  title: {
    default: "rift — agentic development toolkit",
    template: "%s — rift",
  },
  description:
    "Rift is an agentic development toolkit for reading, discovering and editing codebases.",
  // Next does not apply next.config `basePath` to metadata icons, so it has
  // to be prefixed by hand or the favicon 404s under /rift.
  icons: { icon: `${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/logo.svg` },
};

export default function Layout({ children }: LayoutProps<"/">) {
  return (
    <html lang="en" className={cn(inter.variable, geistMono.variable)} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen">
        {/*
          `type: "static"` because `output: "export"` has no server to answer a
          search request: app/api/search pre-renders the Orama index to a file
          and the client downloads it once and queries it in the browser.
          `api` is the download URL, and it needs the basePath spelled out —
          it defaults to a bare "/api/search", which 404s under /rift.
        */}
        <RootProvider
          search={{
            options: {
              type: "static",
              api: `${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/api/search`,
            },
          }}
        >
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
