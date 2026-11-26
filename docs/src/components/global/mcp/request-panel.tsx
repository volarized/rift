"use client";

import { CircleNotchIcon } from "@phosphor-icons/react";
import { useMemo } from "react";

import { argumentsFor, type ToolExample } from "@/components/global/mcp/examples";
import { Elapsed } from "@/components/global/mcp/result-view";
import {
  type Arguments,
  argumentExamples,
  argumentsFromForm,
  type FormState,
  fieldSpecs,
  formFromArguments,
  parseArguments,
} from "@/components/global/mcp/schema-form";
import { ArgumentsEditor, fieldId, ToolForm } from "@/components/global/mcp/tool-form";
import type { ToolInfo } from "@/components/global/mcp/types";
import {
  CODE,
  EmptyState,
  LABEL,
  PANEL_TAB,
  PANEL_TABS,
  PanelHeader,
} from "@/components/global/mcp/ui";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

/** What the visitor has typed for one tool. The console keeps one per tool. */
export type Editor = {
  mode: "form" | "json";
  form: FormState;
  /** Text of JSON mode. */
  json: string;
  /** A call was attempted, so empty required fields are marked. */
  attempted: boolean;
  /** Why the last switch between the two modes did not happen. */
  notice?: string;
};

export const EMPTY_EDITOR: Editor = {
  mode: "form",
  form: { fields: {}, extra: {} },
  json: "{}",
  attempted: false,
};

const OUTLINE = "font-mono font-normal";

/**
 * The form of the selected tool and the controls that run it. Both modes edit
 * the same arguments: a switch converts what the other mode held, and refuses
 * while that does not parse.
 */
export function RequestPanel({
  className,
  editor,
  examples: named,
  onCall,
  onCancel,
  onEditorChange,
  pendingSince,
  tool,
}: {
  className?: string;
  editor: Editor;
  /** Named calls for the selected tool; without them the examples of its schema are offered. */
  examples?: ToolExample[];
  onCall: (args: Arguments) => void;
  onCancel: () => void;
  onEditorChange: (editor: Editor) => void;
  /** Start of the call in flight, from `performance.now()`. */
  pendingSince?: number;
  tool?: ToolInfo;
}) {
  const specs = useMemo(() => (tool ? fieldSpecs(tool.inputSchema) : []), [tool]);
  const examples = useMemo<ToolExample[]>(() => {
    if (named && named.length > 0) return named;
    const fromSchema = tool ? argumentExamples(tool.inputSchema) : [];
    return fromSchema.map((example, index) => ({
      label: fromSchema.length > 1 ? `Use example ${index + 1}` : "Use example",
      arguments: example,
    }));
  }, [named, tool]);
  const fromPage = named !== undefined && named.length > 0;
  const pending = pendingSince !== undefined;

  const read = argumentsFromForm(specs, editor.form);
  // An empty required field is an error only once a call was attempted.
  const formErrors = Object.fromEntries(
    Object.entries(read.errors).filter(([, error]) => editor.attempted || error !== "Required."),
  );
  const parsed = parseArguments(editor.json);

  const setMode = (mode: Editor["mode"]) => {
    if (mode === editor.mode) return;
    if (mode === "json") {
      const blocking = Object.values(read.errors).some((error) => error !== "Required.");
      if (blocking) {
        onEditorChange({
          ...editor,
          notice: "Correct the marked fields before switching to JSON.",
        });
        return;
      }
      onEditorChange({
        ...editor,
        mode,
        json: JSON.stringify(read.args, null, 2),
        notice: undefined,
      });
      return;
    }
    if (!parsed.args) {
      onEditorChange({ ...editor, notice: "Correct the JSON before switching to the form." });
      return;
    }
    onEditorChange({
      ...editor,
      mode,
      form: formFromArguments(specs, parsed.args),
      notice: undefined,
    });
  };

  const submit = () => {
    if (!tool || pending) return;
    if (editor.mode === "json") {
      if (parsed.args) onCall(parsed.args);
      else document.getElementById(fieldId(tool.name, "$arguments"))?.focus();
      return;
    }
    const invalid = specs.find((spec) => read.errors[spec.name]);
    if (invalid) {
      onEditorChange({ ...editor, attempted: true });
      document.getElementById(fieldId(tool.name, invalid.name))?.focus();
      return;
    }
    onCall(read.args);
  };

  return (
    <section aria-label="Tool call" className={cn("flex min-h-0 min-w-0 flex-col", className)}>
      <Tabs
        value={editor.mode}
        onValueChange={(value) => setMode(value as Editor["mode"])}
        className="gap-0"
      >
        <PanelHeader title="Call">
          <TabsList
            variant="line"
            aria-label="Arguments editor"
            className={cn(PANEL_TABS, "mr-1.5 ml-auto")}
          >
            <TabsTrigger value="form" disabled={!tool} className={PANEL_TAB}>
              Form
            </TabsTrigger>
            <TabsTrigger value="json" disabled={!tool} className={PANEL_TAB}>
              JSON
            </TabsTrigger>
          </TabsList>
        </PanelHeader>
      </Tabs>
      {tool ? (
        <form
          noValidate
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
          className="flex min-h-0 flex-1 flex-col"
        >
          <ScrollArea className="min-h-0 flex-1 max-lg:*:data-[slot=scroll-area-viewport]:max-h-[32rem]">
            <div className="grid gap-5 p-3">
              <div className="grid gap-1.5">
                <h4 className={cn(CODE, "break-all")}>{tool.name}</h4>
                {tool.title && tool.title !== tool.name ? (
                  <p className="text-xs">{tool.title}</p>
                ) : null}
                {tool.description ? (
                  <p className="text-xs leading-relaxed whitespace-pre-line text-muted-foreground">
                    {tool.description}
                  </p>
                ) : null}
              </div>
              {examples.length > 0 ? (
                <div className="flex flex-wrap items-center gap-2">
                  <span className={LABEL}>
                    {fromPage ? "Examples" : "Arguments from the schema"}
                  </span>
                  {examples.map((example) => (
                    <Button
                      key={example.label}
                      type="button"
                      variant="outline"
                      size="xs"
                      onClick={() => {
                        const args = argumentsFor(example, specs);
                        onEditorChange({
                          ...editor,
                          form: formFromArguments(specs, args),
                          json: JSON.stringify(args, null, 2),
                          notice: undefined,
                        });
                      }}
                      className={OUTLINE}
                    >
                      {example.label}
                    </Button>
                  ))}
                </div>
              ) : null}
              {editor.mode === "form" ? (
                <ToolForm
                  tool={tool.name}
                  specs={specs}
                  form={editor.form}
                  errors={formErrors}
                  onChange={(form) => onEditorChange({ ...editor, form, notice: undefined })}
                />
              ) : (
                <ArgumentsEditor
                  tool={tool.name}
                  value={editor.json}
                  error={parsed.error}
                  onChange={(json) => onEditorChange({ ...editor, json, notice: undefined })}
                />
              )}
            </div>
          </ScrollArea>
          <div className="flex min-h-12 shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-t border-border px-3 py-2">
            <Button type="submit" size="sm" disabled={pending} className="font-mono max-md:h-9">
              {pending ? (
                <CircleNotchIcon
                  aria-hidden="true"
                  className="motion-safe:animate-spin motion-reduce:opacity-60"
                />
              ) : null}
              Call tool
            </Button>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!pending}
              onClick={onCancel}
              className={cn(OUTLINE, "max-md:h-9")}
            >
              Cancel
            </Button>
            <p aria-live="polite" className="min-w-0 flex-1 text-xs text-muted-foreground">
              {editor.notice ? (
                <span className="text-destructive">{editor.notice}</span>
              ) : pending ? (
                <Elapsed startedAt={pendingSince} />
              ) : null}
            </p>
            <p className="font-mono text-[0.6875rem] text-muted-foreground max-sm:hidden">
              Enter calls, Esc cancels
            </p>
          </div>
        </form>
      ) : (
        <EmptyState>Select a tool to build a call</EmptyState>
      )}
    </section>
  );
}
