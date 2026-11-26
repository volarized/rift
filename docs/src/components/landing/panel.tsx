"use client";

import { animated, easings, useSpring } from "@react-spring/web";
import { Header } from "fumadocs-ui/layouts/home/slots/header";
import { usePathname } from "next/navigation";
import { type ComponentProps, type ReactNode, useEffect, useRef } from "react";
import { cn } from "@/lib/utils";

export function LandingPanel({ children, className }: { children: ReactNode; className?: string }) {
  const panel = useRef<HTMLDivElement>(null);
  const [{ extent, opacity }, api] = useSpring(() => ({ extent: 1, opacity: 1 }));

  useEffect(() => {
    const element = panel.current;
    const preference = window.matchMedia("(prefers-reduced-motion: reduce)");
    if (!element || preference.matches || !window.IntersectionObserver) return;

    api.set({ extent: 0, opacity: 0 });
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (!entry.isIntersecting) return;

        observer.disconnect();
        void api.start({
          to: async (next) => {
            await next({ extent: 1, config: { duration: 430, easing: easings.easeOutCubic } });
            await next({ opacity: 1, config: { tension: 190, friction: 24, clamp: true } });
          },
        });
      },
      { threshold: 0.16 },
    );
    observer.observe(element);

    const finish = () => {
      observer.disconnect();
      api.stop();
      api.set({ extent: 1, opacity: 1 });
    };

    element.addEventListener("focusin", finish, { once: true });
    preference.addEventListener("change", finish, { once: true });
    return () => {
      element.removeEventListener("focusin", finish);
      preference.removeEventListener("change", finish);
      finish();
    };
  }, [api]);

  const size = extent.to((value) => `calc(${value * 100}% + ${(1 - value) * 18}px)`);

  return (
    <div ref={panel} className={cn("landing-panel", className)}>
      <animated.div
        className="landing-panel__frame"
        aria-hidden="true"
        style={{ width: size, height: size }}
      />
      <animated.div className="landing-panel__content" style={{ opacity }}>
        {children}
      </animated.div>
    </div>
  );
}

export function HomeHeader(props: ComponentProps<"header">) {
  const pathname = usePathname();

  if (pathname !== "/") return <Header {...props} />;

  return (
    <LandingPanel className="landing-header">
      <Header {...props} />
    </LandingPanel>
  );
}

export function HomeFooter({ children }: { children: ReactNode }) {
  const pathname = usePathname();

  if (pathname !== "/") return children;

  return <LandingPanel className="landing-footer">{children}</LandingPanel>;
}
