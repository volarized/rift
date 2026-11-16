"use client";

import type { KeyboardEvent } from "react";

import {
  constraintText,
  type FieldSpec,
  type FieldState,
  type FormState,
  fieldFromValue,
  fieldState,
  placeholderText,
} from "@/components/global/mcp/schema-form";
import { CODE, CONTROL, LABEL } from "@/components/global/mcp/ui";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

const TEXTAREA = cn(CONTROL, "min-h-24 resize-y px-2.5 py-2 leading-relaxed max-md:h-auto");
const LINK_BUTTON =
  "h-6 px-1.5 font-mono text-[0.6875rem] font-normal uppercase tracking-[0.16em] text-muted-foreground underline underline-offset-4 hover:bg-transparent hover:text-foreground dark:hover:bg-transparent";

/** Id of a field's control, so a failed call can move focus to the first invalid one. */
export function fieldId(tool: string, name: string): string {
  return `mcp-arg-${tool}-${name}`;
}

/** `Cmd/Ctrl+Enter` in a textarea submits the form the textarea belongs to. */
export function submitOnModEnter(event: KeyboardEvent<HTMLTextAreaElement>) {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
    event.preventDefault();
    event.currentTarget.form?.requestSubmit();
  }
}

function Field({
  error,
  onChange,
  spec,
  state,
  tool,
}: {
  error?: string;
  onChange: (state: FieldState) => void;
  spec: FieldSpec;
  state: FieldState;
  tool: string;
}) {
  const id = fieldId(tool, spec.name);
  const helpId = `${id}-help`;
  const errorId = `${id}-error`;
  const labelId = `${id}-label`;
  const described = [spec.description ? helpId : null, errorId].filter(Boolean).join(" ");
  const shared = {
    id,
    "aria-describedby": described,
    "aria-invalid": error ? true : undefined,
    "aria-required": spec.required || undefined,
  } as const;
  const asJson = state.asJson || spec.kind === "json";
  const setText = (text: string) => onChange({ ...state, text });

  let control: React.ReactNode;
  if (asJson) {
    control = (
      <Textarea
        {...shared}
        value={state.text}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={submitOnModEnter}
        placeholder={placeholderText(spec)}
        spellCheck={false}
        autoComplete="off"
        rows={4}
        className={TEXTAREA}
      />
    );
  } else if (spec.kind === "boolean") {
    const sent = state.flag ?? (spec.required ? false : null);
    control = (
      <div className="flex min-h-8 items-center gap-3 max-md:min-h-10">
        <Checkbox
          {...shared}
          aria-labelledby={labelId}
          checked={state.flag === true}
          onCheckedChange={(checked) => onChange({ ...state, flag: checked })}
          className="bg-transparent dark:bg-transparent"
        />
        <span className={cn(CODE, sent === null && "text-muted-foreground")}>
          {sent === null ? "not set" : String(sent)}
        </span>
        {state.flag !== null && !spec.required ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={() => onChange({ ...state, flag: null })}
            className={LINK_BUTTON}
          >
            Unset
          </Button>
        ) : null}
      </div>
    );
  } else if (spec.kind === "enum") {
    const items = [
      ...(spec.required ? [] : [{ value: null, label: "not set" }]),
      ...spec.enumValues.map((value, index) => ({
        value: String(index),
        label: typeof value === "string" ? value : JSON.stringify(value),
      })),
    ];
    control = (
      <Select
        items={items}
        value={state.text === "" ? null : state.text}
        onValueChange={(value) => setText(typeof value === "string" ? value : "")}
      >
        <SelectTrigger {...shared} className={cn(CONTROL, "w-full")}>
          <SelectValue placeholder={spec.required ? "Select a value" : "not set"} />
        </SelectTrigger>
        <SelectContent alignItemWithTrigger={false} className="font-mono">
          {items.map((item) => (
            <SelectItem key={item.value ?? "unset"} value={item.value}>
              {item.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    );
  } else {
    const numeric = spec.kind !== "string";
    control = (
      <Input
        {...shared}
        type={numeric ? "number" : "text"}
        inputMode={spec.kind === "integer" ? "numeric" : numeric ? "decimal" : undefined}
        min={numeric ? spec.minimum : undefined}
        max={numeric ? spec.maximum : undefined}
        step={spec.kind === "integer" ? 1 : numeric ? "any" : undefined}
        value={state.text}
        onChange={(event) => setText(event.target.value)}
        placeholder={placeholderText(spec)}
        spellCheck={false}
        autoComplete="off"
        className={CONTROL}
      />
    );
  }

  const example = spec.examples[0];
  return (
    <div className="grid gap-1.5">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5">
        <Label id={labelId} htmlFor={id} className={cn(CODE, "gap-1 font-normal")}>
          {spec.name}
          {spec.required ? (
            <span aria-hidden="true" className="text-muted-foreground">
              *
            </span>
          ) : null}
        </Label>
        <span className="font-mono text-[0.6875rem] text-muted-foreground">
          {constraintText(spec)}
        </span>
        {example !== undefined ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={() => onChange(fieldFromValue(spec, example))}
            className={cn(LINK_BUTTON, "ml-auto")}
          >
            Use example
          </Button>
        ) : null}
      </div>
      {control}
      {state.asJson ? (
        <p className="flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
          This value does not fit the field's type, so the field holds it as JSON.
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={() => onChange({ text: "", flag: null, asJson: false })}
            className={LINK_BUTTON}
          >
            Reset
          </Button>
        </p>
      ) : null}
      {spec.description ? (
        <p id={helpId} className="text-xs leading-relaxed text-muted-foreground">
          {spec.description}
        </p>
      ) : null}
      <p id={errorId} aria-live="polite" className="text-xs text-destructive empty:hidden">
        {error}
      </p>
    </div>
  );
}

/** The form generated from a tool's `inputSchema`. */
export function ToolForm({
  errors,
  form,
  onChange,
  specs,
  tool,
}: {
  errors: Record<string, string>;
  form: FormState;
  onChange: (form: FormState) => void;
  specs: FieldSpec[];
  tool: string;
}) {
  const extra = Object.keys(form.extra);
  return (
    <div className="grid gap-5">
      {specs.length === 0 && extra.length === 0 ? (
        <p className={LABEL}>This tool takes no arguments</p>
      ) : null}
      {specs.map((spec) => (
        <Field
          key={spec.name}
          tool={tool}
          spec={spec}
          state={fieldState(form, spec.name)}
          error={errors[spec.name]}
          onChange={(state) =>
            onChange({ ...form, fields: { ...form.fields, [spec.name]: state } })
          }
        />
      ))}
      {specs.some((spec) => spec.required) ? (
        <p className="font-mono text-[0.6875rem] text-muted-foreground">* required</p>
      ) : null}
      {extra.length > 0 ? (
        <p className="flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
          <span>
            Sent as they are, outside the schema:{" "}
            <code className="font-mono text-foreground">{extra.join(", ")}</code>
          </span>
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={() => onChange({ ...form, extra: {} })}
            className={LINK_BUTTON}
          >
            Remove
          </Button>
        </p>
      ) : null}
    </div>
  );
}

/** JSON mode: the whole `arguments` object as text. */
export function ArgumentsEditor({
  error,
  onChange,
  tool,
  value,
}: {
  error?: string;
  onChange: (value: string) => void;
  tool: string;
  value: string;
}) {
  const id = fieldId(tool, "$arguments");
  return (
    <div className="grid gap-1.5">
      <div className="flex items-baseline gap-3">
        <Label htmlFor={id} className={cn(CODE, "font-normal")}>
          arguments
        </Label>
        <span className="font-mono text-[0.6875rem] text-muted-foreground">object</span>
      </div>
      <Textarea
        id={id}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={submitOnModEnter}
        aria-describedby={`${id}-error`}
        aria-invalid={error ? true : undefined}
        placeholder="{}"
        spellCheck={false}
        autoComplete="off"
        rows={12}
        className={cn(TEXTAREA, "min-h-56")}
      />
      <p id={`${id}-error`} aria-live="polite" className="text-xs text-destructive empty:hidden">
        {error}
      </p>
    </div>
  );
}
