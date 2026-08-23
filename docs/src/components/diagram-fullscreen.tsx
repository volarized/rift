"use client";

import { Dialog } from "@base-ui/react/dialog";
import { Maximize, Maximize2, X, ZoomIn, ZoomOut } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * The interactive shell around a rendered diagram.
 *
 * `FlatDiagram` lays the graph out on the server and hands the finished SVG
 * in as children. This component renders it inline, and on a click opens the
 * same SVG in a fullscreen dialog where it can be panned and zoomed: drag to
 * pan, wheel or pinch to zoom around the cursor, double click or the fit
 * button to frame the whole drawing again.
 */
export type DiagramFullscreenProps = {
  children: React.ReactNode;
  className?: string;
};

/** Pan offset in CSS pixels plus a scale, applied as one CSS transform. */
type View = { scale: number; x: number; y: number };

const FIT: View = { scale: 1, x: 0, y: 0 };
const MIN_SCALE = 0.5;
const MAX_SCALE = 8;
/** How much one press of a zoom button changes the scale. */
const STEP = 1.4;

export function DiagramFullscreen({ children, className }: DiagramFullscreenProps) {
  const [open, setOpen] = useState(false);
  const [view, setView] = useState<View>(FIT);
  const host = useRef<HTMLDivElement>(null);
  const pointers = useRef(new Map<number, { x: number; y: number }>());

  const zoomAt = useCallback((point: { x: number; y: number }, factor: number) => {
    setView((current) => {
      const scale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, current.scale * factor));
      const ratio = scale / current.scale;
      return {
        scale,
        x: point.x - (point.x - current.x) * ratio,
        y: point.y - (point.y - current.y) * ratio,
      };
    });
  }, []);

  const zoomFromCentre = (factor: number) => {
    const element = host.current;
    if (!element) return;
    const rect = element.getBoundingClientRect();
    zoomAt({ x: rect.width / 2, y: rect.height / 2 }, factor);
  };

  // React registers wheel listeners as passive, and a passive listener cannot
  // keep the wheel from scrolling the page. A native listener can.
  useEffect(() => {
    if (!open) return;
    const element = host.current;
    if (!element) return;

    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = element.getBoundingClientRect();
      const point = { x: event.clientX - rect.left, y: event.clientY - rect.top };
      // A trackpad pinch arrives as a wheel event with ctrlKey set, at a much
      // finer delta than a scroll wheel's detents.
      zoomAt(point, Math.exp(-event.deltaY * (event.ctrlKey ? 0.01 : 0.002)));
    };

    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [open, zoomAt]);

  const localPoint = (event: React.PointerEvent<HTMLDivElement>) => {
    const rect = event.currentTarget.getBoundingClientRect();
    return { x: event.clientX - rect.left, y: event.clientY - rect.top };
  };

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    event.currentTarget.setPointerCapture(event.pointerId);
    pointers.current.set(event.pointerId, localPoint(event));
  };

  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const previous = pointers.current.get(event.pointerId);
    if (!previous) return;
    const point = localPoint(event);

    if (pointers.current.size === 2) {
      // A pinch: the other finger anchors, the distance between them scales.
      const anchor = [...pointers.current.entries()].find(([id]) => id !== event.pointerId)?.[1];
      if (anchor) {
        const before = Math.hypot(previous.x - anchor.x, previous.y - anchor.y);
        const after = Math.hypot(point.x - anchor.x, point.y - anchor.y);
        const middle = { x: (point.x + anchor.x) / 2, y: (point.y + anchor.y) / 2 };
        if (before > 0) zoomAt(middle, after / before);
      }
    } else if (pointers.current.size === 1) {
      setView((current) => ({
        ...current,
        x: current.x + point.x - previous.x,
        y: current.y + point.y - previous.y,
      }));
    }

    pointers.current.set(event.pointerId, point);
  };

  const onPointerEnd = (event: React.PointerEvent<HTMLDivElement>) => {
    pointers.current.delete(event.pointerId);
  };

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (next) setView(FIT);
      }}
    >
      <figure className={cn("my-8 min-w-0", className)}>
        <Dialog.Trigger
          render={
            <button
              type="button"
              aria-label="Open the diagram in fullscreen"
              className="group relative block w-full cursor-zoom-in"
            />
          }
        >
          {children}
          <span className="absolute right-2 top-2 grid size-6 place-items-center border border-border bg-background text-muted-foreground opacity-60 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100">
            <Maximize2 className="size-3" aria-hidden="true" />
          </span>
        </Dialog.Trigger>
      </figure>
      <Dialog.Portal>
        <Dialog.Backdrop className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm" />
        <Dialog.Viewport className="fixed inset-0 z-[60] flex p-2 sm:p-4">
          <Dialog.Popup className="flex min-h-0 w-full flex-col overflow-hidden border border-border bg-background shadow-2xl outline-none">
            <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-2">
              <Dialog.Title className="font-mono text-sm font-medium">Diagram</Dialog.Title>
              <div className="flex items-center gap-1">
                <Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Zoom out"
                  onClick={() => zoomFromCentre(1 / STEP)}
                >
                  <ZoomOut aria-hidden="true" />
                </Button>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Zoom in"
                  onClick={() => zoomFromCentre(STEP)}
                >
                  <ZoomIn aria-hidden="true" />
                </Button>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Fit the diagram to the frame"
                  onClick={() => setView(FIT)}
                >
                  <Maximize aria-hidden="true" />
                </Button>
                <Dialog.Close
                  render={
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label="Close the diagram fullscreen view"
                    />
                  }
                >
                  <X aria-hidden="true" />
                </Dialog.Close>
              </div>
            </div>
            {/* biome-ignore lint/a11y/noStaticElementInteractions: a pointer-only pan surface; the header buttons are the keyboard path to the same view */}
            <div
              ref={host}
              className="relative min-h-0 flex-1 cursor-grab touch-none overflow-hidden active:cursor-grabbing"
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerEnd}
              onPointerCancel={onPointerEnd}
              onDoubleClick={() => setView(FIT)}
            >
              <div
                className="pointer-events-none absolute inset-0 select-none p-4 [&_svg]:h-full [&_svg]:w-full"
                style={{
                  transform: `translate(${view.x}px, ${view.y}px) scale(${view.scale})`,
                  transformOrigin: "0 0",
                }}
              >
                {children}
              </div>
            </div>
          </Dialog.Popup>
        </Dialog.Viewport>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
