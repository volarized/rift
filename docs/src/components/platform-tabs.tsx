"use client";

import { Tabs } from "fumadocs-ui/components/tabs";
import { type ReactNode, useEffect, useState } from "react";

const items = ["Linux and macOS", "Windows PowerShell"];

// Server rendering defaults to the first tab; after hydration the effect
// switches to the Windows tab when the user agent names Windows. Tabs takes
// no controlled value, so the key remounts it with the new default.
export function PlatformTabs({ children }: { children: ReactNode }) {
  const [index, setIndex] = useState(0);

  useEffect(() => {
    if (navigator.userAgent.includes("Windows")) {
      setIndex(1);
    }
  }, []);

  return (
    <Tabs key={index} items={items} defaultIndex={index}>
      {children}
    </Tabs>
  );
}
