import { ArrowRightIcon } from "@phosphor-icons/react/dist/ssr";
import Link from "next/link";

import { LandingMap } from "@/components/landing/map";
import { LandingPanel } from "@/components/landing/panel";
import { Logo } from "@/components/logo";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import "./landing.css";

const DIFFERENCES = [
  {
    id: "language-providers",
    statement: "Code facts with symbol history.",
    title: "Programming language providers",
    body: "Rift combines syntax and symbol history to give agents context about the code they read.",
  },
  {
    id: "dependencies",
    statement: "Source from the project and its dependencies.",
    title: "Dependency discovery",
    body: "Rift works in accordance with your project context and dependencies, providing details about them for your agents.",
  },
  {
    id: "mcp",
    statement: "Structured code reads.",
    title: "MCP tools and resources",
    body: "Agents find declarations, inspect syntax, and follow edges through MCP tools.",
  },
];

function Statement({ children }: { children: React.ReactNode }) {
  return (
    <h2 className="m-0 text-[clamp(1.65rem,3vw,2.8rem)] leading-[1.13] font-semibold tracking-tight text-balance">
      {children}
    </h2>
  );
}

function Body({ children }: { children: React.ReactNode }) {
  return (
    <p className="m-0 text-base leading-relaxed text-pretty text-muted-foreground sm:text-lg">
      {children}
    </p>
  );
}

function Stop({
  children,
  id,
  alternate = false,
  illustration,
}: {
  children: React.ReactNode;
  id: string;
  alternate?: boolean;
  illustration?: React.ReactNode;
}) {
  return (
    <section
      id={id}
      data-map-stop=""
      className={cn(
        "landing-stop",
        alternate && "landing-stop--alternate",
        illustration && "landing-stop--hero",
      )}
    >
      {illustration}
      <LandingPanel className="landing-stop__panel">
        <div className="grid gap-4 p-5 sm:gap-5 sm:p-7 lg:p-9">{children}</div>
      </LandingPanel>
    </section>
  );
}

export default function HomePage() {
  return (
    <div className="landing-page">
      <LandingMap />
      <Stop
        id="overview"
        illustration={
          <LandingPanel className="landing-stop__mark">
            <Logo
              aria-hidden="true"
              strokeWidth={0.37}
              className="w-full p-6 text-primary-text sm:p-9 lg:p-12"
            />
          </LandingPanel>
        }
      >
        <h1 className="m-0 font-mono text-[clamp(3.5rem,8.5vw,7rem)] leading-[0.88] font-bold tracking-[-0.075em] dark:text-white">
          rift
        </h1>
        <Body>
          Rift is an agentic development toolkit for reading, discovering, and exploring codebases.
        </Body>
      </Stop>

      <Stop id="context" alternate>
        <Statement>Agents depend on context and tooling.</Statement>
        <Body>rift provides contextual, precise and well-structured codebase reading tools.</Body>
      </Stop>

      {DIFFERENCES.map((item, index) => (
        <Stop key={item.id} id={item.id} alternate={index === 1}>
          <Statement>{item.statement}</Statement>
          <div className="grid gap-2">
            <h3 className="m-0 font-mono text-xs leading-relaxed font-medium text-primary-text sm:text-sm">
              {item.title}
            </h3>
            <Body>{item.body}</Body>
          </div>
        </Stop>
      ))}

      <Stop id="get-started" alternate>
        <Statement>Bring understanding and cohesiveness to your agentic workflow</Statement>
        <div className="flex flex-wrap items-center gap-2 sm:gap-3">
          <Link
            href="/docs/installation"
            className={cn(
              buttonVariants({ size: "lg" }),
              "font-mono font-semibold tracking-wide dark:shadow-[0_0_24px] dark:shadow-primary/35",
            )}
          >
            Install
            <ArrowRightIcon weight="bold" className="h-4 w-4" />
          </Link>
          <Link
            href="/docs"
            className={cn(
              buttonVariants({ variant: "outline", size: "lg" }),
              "font-mono tracking-wide",
            )}
          >
            Read the docs
            <ArrowRightIcon weight="bold" className="h-4 w-4 text-primary-text" />
          </Link>
        </div>
      </Stop>
    </div>
  );
}
