import Link from "next/link";
import { ArrowRightIcon } from "@phosphor-icons/react/dist/ssr";

import { Command } from "@/components/command";
import { IsoStack, type IsoLayer } from "@/components/iso-stack";
import { LineObject, type LineObjectKind } from "@/components/line-object";
import { Logo } from "@/components/logo";
import { Reveal } from "@/components/reveal";
import { ExplodedObject, type ExplodedKind } from "@/components/exploded-object";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const SHELL = "mx-auto w-full max-w-190 px-6 sm:px-7";
const WIDE = "mx-auto w-full max-w-280 px-6 sm:px-7";

/** Every section is a screen, less the header it scrolls under. */
const SCREEN = "min-h-[calc(80svh)]";

/** Inter 300 at display size — one statement per section, never more. */
function Statement({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <p
      className={cn(
        "m-0 max-w-165 font-sans leading-[1.28] font-light tracking-[-0.015em] text-4xl text-balance text-foreground",
        className,
      )}
    >
      {children}
    </p>
  );
}

/** Inter 300, 16/1.6 — body copy. */
function Body({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <p
      className={cn(
        "m-0 max-w-150 leading-relaxed font-light text-pretty text-muted-foreground",
        className,
      )}
    >
      {children}
    </p>
  );
}

function Section({
  children,
  className,
  wide,
  id,
}: {
  children: React.ReactNode;
  className?: string;
  wide?: boolean;
  id?: string;
}) {
  return (
    <section id={id} className={cn("flex scroll-mt-14 items-center", SCREEN)}>
      <Reveal className={cn(wide ? WIDE : SHELL, "grid gap-9 py-16", className)}>{children}</Reveal>
    </section>
  );
}

const LAYERS: IsoLayer[] = [
  { label: "your codebase" },
  { label: "your agent" },
  { label: "rift", note: "the toolkit for your agent" },
  {
    label: "result",
    note: "token-efficient reads and edits, with full introspection",
  },
];

/**
 * One object per point: the compiler is the solid taken apart, the other two
 * are attractors — a reversible system and a dynamo that swaps between two
 * states.
 */
const DIFFERENCES: {
  title: string;
  body: string;
  solid?: ExplodedKind;
  line?: LineObjectKind;
  tilt?: number;
  spin?: number;
  fit?: number;
}[] = [
  {
    title: "Direct compiler integration",
    body: "rift talks to the language’s own compiler, so every read is typed and every edit is validated before it lands.",
    solid: "sphere",
  },
  {
    title: "Reversible, git-backed worktrees",
    body: "Every change an agent makes is staged in a worktree.",
    line: "shimizu",
    // The butterfly stands in the x–z plane; turn it to face the camera.
    tilt: 1.5,
  },
  {
    title: "Structured reads and writes over MCP",
    body: "Tools and resources instead of shell transcripts. Agents work with code semantics and edit it structurally.",
    line: "tsucs1",
    tilt: 0.9,
    spin: 0.4,
    // A deep body, not a flat one: stand back a little further than the rest.
    fit: 1,
  },
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col">
      {/* 1 — hero, standing on a warped ground plane */}
      <section className={cn("relative flex items-center overflow-hidden", SCREEN)}>
        <div
          className={cn(
            SHELL,
            "relative grid items-center gap-12 py-16 md:grid-cols-[1fr_auto] md:gap-16",
          )}
        >
          <div className="grid justify-items-start gap-5">
            <h1 className="m-0 font-mono text-[52px] leading-none font-medium tracking-[-0.03em] sm:text-[64px]">
              rift
            </h1>

            <Body className="max-w-115">
              Agentic development toolkit for reading, discovering and editing codebases.
            </Body>
          </div>

          {/* Stitched to the scroll: it draws itself in on the way down the
              hero and unpicks itself as the hero leaves. */}
          <Logo
            bloom
            strokeWidth={0.24}
            className="h-56 w-full max-w-56 justify-self-center text-foreground/70 sm:h-72 sm:max-w-72 md:justify-self-end"
          />
        </div>
      </section>

      {/* 2 — the claim, over a soft Lorenz attractor */}
      <Section wide className="md:grid-cols-[auto_1fr] md:items-center">
        <LineObject
          kind="lorenz"
          reveal
          // The butterfly lies in the x–z plane; stand it up to face the camera.
          tilt={1.48}
          fit={0.8}
          opacity={0.15}
          className="w-full min-w-96 justify-self-center sm:h-72 md:w-72"
        />
        <div className="grid gap-5">
          <Statement>Agents depend on context and tooling.</Statement>
          <Body className="text-balance">
            rift provides typesafe, token-efficient, compiler-backed reads and edits.
          </Body>
        </div>
      </Section>

      {/* 4 — the design position, one point per screen, each against the same
          sphere taken apart on a different plane */}
      {DIFFERENCES.map((item, index) => (
        <Section key={item.title} wide className="md:grid-cols-2 md:items-center text-base">
          <div className="grid max-w-165 gap-7">
            {index === 0 ? (
              <Statement className="max-w-125">
                Designed with simplicity and minimalism in mind.
              </Statement>
            ) : null}
            <div className="grid gap-3">
              <h3 className="m-0 font-mono font-medium tracking-[0.02em]">{item.title}</h3>
              <Body>{item.body}</Body>
            </div>
          </div>

          {/* Composed on the way in, apart at the centre line, composed
              again on the way out. */}
          {item.solid ? (
            <ExplodedObject
              kind={item.solid}
              className={cn(
                "h-56 w-full max-w-64 justify-self-center sm:h-72 md:w-72",
                index % 2 === 1 && "md:order-first",
              )}
            />
          ) : (
            <LineObject
              kind={item.line!}
              reveal
              tilt={item.tilt}
              spin={item.spin}
              fit={item.fit ?? 0.78}
              opacity={0.15}
              className={cn(
                "h-56 w-full max-w-64 justify-self-center sm:h-72 md:w-72",
                index % 2 === 1 && "md:order-first",
              )}
            />
          )}
        </Section>
      ))}

      {/* 7 — the two ways out, against the Aizawa attractor drawn on by the
          scroll and unpicked again on the way back up */}
      <Section wide className="md:grid-cols-2 md:items-center md:gap-20">
        <div className="grid gap-9">
          <Statement className="max-w-125">
            Coming soon, reach out if you want to get involved.
          </Statement>
          <Link
            href="mailto:contact@volar.sh"
            className="flex items-center gap-2 text-sm font-medium w-0"
          >
            <Button variant="outline" size="lg" className="w-fit">
              Get in touch
              <ArrowRightIcon weight="bold" className="h-4 w-4" />
            </Button>
          </Link>
        </div>

        {/* The disk lies in x–y with the tube down z: face it near-on, so the
            trace reads as a vortex rather than a smear. */}
        <LineObject
          kind="aizawa"
          reveal
          tilt={0.26}
          spin={-0.55}
          fit={0.82}
          opacity={0.14}
          className="h-56 w-full max-w-64 justify-self-center sm:h-72 md:w-72"
        />
      </Section>
    </main>
  );
}
