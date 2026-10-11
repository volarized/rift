"use client";

import {
  AsteriskIcon,
  BracketsCurlyIcon,
  CpuIcon,
  FileTextIcon,
  GearIcon,
  MagnifyingGlassIcon,
  MinusIcon,
} from "@phosphor-icons/react";
import { type ComponentType, type SVGProps, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Angular } from "@/components/ui/svgs/angular";
import { CssOld } from "@/components/ui/svgs/cssOld";
import { Html5 } from "@/components/ui/svgs/html5";
import { Json } from "@/components/ui/svgs/json";
import { MarkdownLight } from "@/components/ui/svgs/markdownLight";
import { Svelte } from "@/components/ui/svgs/svelte";
import { Toml } from "@/components/ui/svgs/toml";
import { Vue } from "@/components/ui/svgs/vue";

type Language = {
  name: string;
  logo?: string;
  icon?: ComponentType<SVGProps<SVGSVGElement>>;
  syntax: boolean;
  embedded?: boolean;
};

const LANGUAGES: Language[] = [
  { name: "Any language", syntax: false },
  { name: "Rust", logo: "rust", syntax: true },
  { name: "Python", logo: "python", syntax: true, embedded: true },
  { name: "JavaScript", logo: "javascript", syntax: true },
  { name: "TypeScript", logo: "typescript", syntax: true },
  { name: "TypeScript (TSX)", logo: "typescript", syntax: true },
  { name: "HTML", icon: Html5, syntax: true },
  { name: "Angular", icon: Angular, syntax: true },
  { name: "CSS", icon: CssOld, syntax: true },
  { name: "Vue", icon: Vue, syntax: true },
  { name: "Svelte", icon: Svelte, syntax: true },
  { name: "C", logo: "c", syntax: true },
  { name: "C++", logo: "c-plusplus", syntax: true },
  { name: "Cython", syntax: true },
  { name: "Markdown", icon: MarkdownLight, syntax: true },
  { name: "JSON", icon: Json, syntax: true },
  { name: "JSONC", icon: Json, syntax: true },
  { name: "YAML", syntax: true },
  { name: "TOML", icon: Toml, syntax: true },
];

function LanguageIcon({ language }: { language: Language }) {
  if (language.icon) {
    const Icon = language.icon;
    return (
      <Icon
        width={18}
        height={18}
        aria-hidden="true"
        className={`shrink-0 grayscale ${language.name === "Markdown" || language.name.startsWith("JSON") ? "dark:invert" : ""}`}
      />
    );
  }
  if (!language.logo) {
    const Icon = language.syntax ? FileTextIcon : AsteriskIcon;
    return <Icon size={18} weight="light" aria-hidden="true" />;
  }
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" aria-hidden="true">
      <image
        href={`${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/icons/svgl/${language.logo}.svg`}
        width="24"
        height="24"
        className={
          ["javascript", "typescript", "c", "c-plusplus"].includes(language.logo)
            ? "grayscale"
            : "brightness-0 dark:invert"
        }
      />
    </svg>
  );
}

export function CodebaseLanguages() {
  const [query, setQuery] = useState("");
  const search = query.trim().toLowerCase();
  const languages = LANGUAGES.filter((language) => language.name.toLowerCase().includes(search));

  return (
    <section aria-label="Languages and providers" className="not-prose my-6 border border-border">
      <label className="flex items-center gap-2 border-b border-border px-4 py-3">
        <MagnifyingGlassIcon size={16} weight="light" aria-hidden="true" />
        <input
          type="search"
          aria-label="Search languages"
          placeholder="Search languages"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring"
        />
      </label>
      <section
        aria-label="Language support"
        // biome-ignore lint/a11y/noNoninteractiveTabindex: keyboard users need focus to scroll the language table.
        tabIndex={0}
        className="max-h-72 overflow-auto focus-visible:outline-2 focus-visible:outline-ring"
      >
        <table className="w-full text-left text-xs">
          <caption className="sr-only">Syntax providers and language engines</caption>
          <thead className="sticky top-0 z-10 bg-background font-mono text-muted-foreground">
            <tr>
              <th scope="col" className="px-4 py-2 font-normal">
                Language
              </th>
              <th scope="col" className="px-3 py-2 font-normal">
                Syntax
              </th>
              <th scope="col" className="px-4 py-2 font-normal">
                LSP
              </th>
            </tr>
          </thead>
          <tbody>
            {languages.map((language) => (
              <tr key={language.name} className="border-t border-border hover:bg-muted/30">
                <th scope="row" className="px-4 py-2 font-normal">
                  <span className="flex items-center gap-3 whitespace-nowrap">
                    <LanguageIcon language={language} />
                    {language.name}
                  </span>
                </th>
                <td className="px-3 py-2">
                  {language.syntax ? (
                    <span className="inline-flex items-center">
                      <BracketsCurlyIcon size={16} weight="light" aria-hidden="true" />
                      <span className="sr-only">Supported</span>
                    </span>
                  ) : (
                    <span>
                      <MinusIcon size={16} weight="light" aria-hidden="true" />
                      <span className="sr-only">No syntax provider</span>
                    </span>
                  )}
                </td>
                <td className="whitespace-nowrap px-4 py-2 text-muted-foreground">
                  <span className="flex items-center gap-2">
                    {language.embedded && (
                      <Badge variant="outline" className="text-[10px]">
                        <CpuIcon weight="light" aria-hidden="true" data-icon="inline-start" />
                        Embedded ty
                      </Badge>
                    )}
                    <Badge variant="outline" className="text-[10px]">
                      <GearIcon weight="light" aria-hidden="true" data-icon="inline-start" />
                      Via configuration
                    </Badge>
                  </span>
                </td>
              </tr>
            ))}
            {languages.length === 0 && (
              <tr>
                <td colSpan={3} className="px-4 py-6 text-muted-foreground">
                  No matching languages.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </section>
      <p className="border-t border-border px-4 py-3 text-xs leading-relaxed text-muted-foreground">
        Every visible UTF-8 file supports text search. LSP requires a configured language engine;
        Python can also use the embedded ty engine.
      </p>
    </section>
  );
}
