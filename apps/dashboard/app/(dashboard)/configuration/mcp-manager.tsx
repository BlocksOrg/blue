"use client";

import { useEffect, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export type SupportedHarness = string;

export type McpServer = {
  name: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  transport?: string;
  disabled?: boolean;
};

export type McpByHarness = Partial<Record<SupportedHarness, McpServer[]>>;
export type McpDefinition = { server: McpServer; harnesses: SupportedHarness[] };

function normalized(server: McpServer) {
  return {
    name: server.name,
    command: server.command,
    args: server.args ?? [],
    env: Object.fromEntries(Object.entries(server.env ?? {}).sort(([a], [b]) => a.localeCompare(b))),
    url: server.url,
    transport: server.transport,
    disabled: server.disabled ?? false,
  };
}

export function groupMcpServers(mcp: McpByHarness, supportedHarnesses = Object.keys(mcp)): McpDefinition[] {
  const grouped = new Map<string, McpDefinition>();
  supportedHarnesses.forEach((harness) => {
    (mcp[harness] ?? []).forEach((server) => {
      const clean = normalized(server);
      const signature = JSON.stringify(clean);
      const current = grouped.get(signature);
      if (current) current.harnesses.push(harness);
      else grouped.set(signature, { server: clean, harnesses: [harness] });
    });
  });
  return [...grouped.values()];
}

export function expandMcpDefinitions(definitions: McpDefinition[], supportedHarnesses: string[]): McpByHarness {
  const result: McpByHarness = {};
  supportedHarnesses.forEach((harness) => {
    result[harness] = definitions
      .filter((definition) => definition.harnesses.includes(harness))
      .map((definition) => normalized(definition.server));
  });
  return result;
}

type EnvRow = { key: string; value: string };
type Draft = {
  name: string;
  kind: "local" | "remote";
  command: string;
  args: string[];
  env: EnvRow[];
  url: string;
  transport: string;
  disabled: boolean;
  harnesses: SupportedHarness[];
};

const emptyDraft: Draft = {
  name: "",
  kind: "local",
  command: "",
  args: [],
  env: [],
  url: "",
  transport: "",
  disabled: false,
  harnesses: [],
};

function toDraft(definition?: McpDefinition): Draft {
  if (!definition) return emptyDraft;
  const server = normalized(definition.server);
  return {
    name: server.name,
    kind: server.url ? "remote" : "local",
    command: server.command ?? "",
    args: server.args,
    env: Object.entries(server.env).map(([key, value]) => ({ key, value })),
    url: server.url ?? "",
    transport: server.transport ?? "",
    disabled: server.disabled,
    harnesses: definition.harnesses,
  };
}

export function McpEditorDialog({
  open,
  definition,
  definitions,
  editingIndex,
  onOpenChange,
  onSave,
  supportedHarnesses,
}: {
  open: boolean;
  definition?: McpDefinition;
  definitions: McpDefinition[];
  editingIndex?: number;
  onOpenChange: (open: boolean) => void;
  onSave: (definition: McpDefinition) => void;
  supportedHarnesses: string[];
}) {
  const [draft, setDraft] = useState<Draft>(emptyDraft);
  const [error, setError] = useState("");

  useEffect(() => {
    if (open) {
      setDraft(toDraft(definition));
      setError("");
    }
  }, [open, definition]);

  function updateArgument(index: number, value: string) {
    setDraft((current) => ({ ...current, args: current.args.map((item, itemIndex) => itemIndex === index ? value : item) }));
  }
  function updateEnv(index: number, field: keyof EnvRow, value: string) {
    setDraft((current) => ({ ...current, env: current.env.map((item, itemIndex) => itemIndex === index ? { ...item, [field]: value } : item) }));
  }
  function toggleHarness(harness: SupportedHarness, checked: boolean) {
    setDraft((current) => ({
      ...current,
      harnesses: checked
        ? [...new Set([...current.harnesses, harness])]
        : current.harnesses.filter((value) => value !== harness),
    }));
  }
  function save() {
    const name = draft.name.trim();
    if (!name) return setError("Enter a server name.");
    if (!draft.harnesses.length) return setError("Select at least one agent.");
    if (draft.kind === "local" && !draft.command.trim()) return setError("Enter the MCP command.");
    if (draft.kind === "remote") {
      try {
        const url = new URL(draft.url);
        if (!["http:", "https:"].includes(url.protocol)) throw new Error();
      } catch {
        return setError("Enter a valid HTTP or HTTPS MCP URL.");
      }
    }
    const collision = definitions.some((current, index) =>
      index !== editingIndex &&
      current.server.name.trim() === name &&
      current.harnesses.some((harness) => draft.harnesses.includes(harness)),
    );
    if (collision) return setError(`A server named ${name} already targets one of the selected agents.`);
    const server: McpServer = draft.kind === "local"
      ? {
          name,
          command: draft.command.trim(),
          args: draft.args.map((value) => value.trim()).filter(Boolean),
          env: Object.fromEntries(draft.env.map((row) => [row.key.trim(), row.value]).filter(([key]) => Boolean(key))),
          disabled: draft.disabled,
        }
      : {
          name,
          url: draft.url.trim(),
          transport: draft.transport.trim() || undefined,
          disabled: draft.disabled,
        };
    onSave({ server, harnesses: draft.harnesses });
    onOpenChange(false);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[calc(100svh-2rem)] grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{definition ? "Edit MCP server" : "Add MCP server"}</DialogTitle>
          <DialogDescription>Configure one server and choose which supported agents receive it.</DialogDescription>
        </DialogHeader>
        <div className="grid gap-6 overflow-y-auto pr-1">
          <div className="grid gap-2">
            <Label htmlFor="mcp-name">Server name</Label>
            <Input id="mcp-name" value={draft.name} onChange={(event) => setDraft({ ...draft, name: event.target.value })} placeholder="team-tools" />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="mcp-kind">Connection type</Label>
            <Select value={draft.kind} onValueChange={(value) => setDraft({ ...draft, kind: value as Draft["kind"] })}>
              <SelectTrigger id="mcp-kind" className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent><SelectItem value="local">Local command</SelectItem><SelectItem value="remote">Remote URL</SelectItem></SelectContent>
            </Select>
          </div>
          {draft.kind === "local" ? <>
            <div className="grid gap-2"><Label htmlFor="mcp-command">Command</Label><Input id="mcp-command" value={draft.command} onChange={(event) => setDraft({ ...draft, command: event.target.value })} placeholder="npx" /></div>
            <section className="grid gap-3">
              <div className="flex items-center justify-between gap-3"><div className="min-w-0"><h3 className="font-medium">Arguments</h3><p className="text-xs break-words text-muted-foreground">Arguments are passed in the displayed order.</p></div><Button type="button" size="sm" variant="outline" onClick={() => setDraft({ ...draft, args: [...draft.args, ""] })}><Plus /> Add argument</Button></div>
              {draft.args.map((argument, index) => <div className="flex min-w-0 gap-2" key={index}><Input aria-label={`Argument ${index + 1}`} value={argument} onChange={(event) => updateArgument(index, event.target.value)} /><Button type="button" size="icon" variant="ghost" aria-label={`Remove argument ${index + 1}`} onClick={() => setDraft({ ...draft, args: draft.args.filter((_, itemIndex) => itemIndex !== index) })}><Trash2 /></Button></div>)}
            </section>
            <section className="grid gap-3">
              <div className="flex items-center justify-between gap-3"><div className="min-w-0"><h3 className="font-medium">Environment</h3><p className="text-xs break-words text-muted-foreground">Values are delivered to managed clients and are not stored as secrets.</p></div><Button type="button" size="sm" variant="outline" onClick={() => setDraft({ ...draft, env: [...draft.env, { key: "", value: "" }] })}><Plus /> Add variable</Button></div>
              {draft.env.map((row, index) => <div className="grid min-w-0 gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]" key={index}><Input aria-label={`Environment variable ${index + 1} name`} value={row.key} placeholder="VARIABLE_NAME" onChange={(event) => updateEnv(index, "key", event.target.value)} /><Input aria-label={`Environment variable ${index + 1} value`} value={row.value} placeholder="value" onChange={(event) => updateEnv(index, "value", event.target.value)} /><Button type="button" size="icon" variant="ghost" aria-label={`Remove environment variable ${index + 1}`} onClick={() => setDraft({ ...draft, env: draft.env.filter((_, itemIndex) => itemIndex !== index) })}><Trash2 /></Button></div>)}
            </section>
          </> : <>
            <div className="grid gap-2"><Label htmlFor="mcp-url">Remote URL</Label><Input id="mcp-url" type="url" value={draft.url} onChange={(event) => setDraft({ ...draft, url: event.target.value })} placeholder="https://mcp.example.com/api" /></div>
            <div className="grid gap-2"><Label htmlFor="mcp-transport">Transport (optional)</Label><Input id="mcp-transport" value={draft.transport} onChange={(event) => setDraft({ ...draft, transport: event.target.value })} placeholder="streamable-http" /></div>
          </>}
          <fieldset className="grid gap-3"><legend className="font-medium">Agents</legend><p className="text-xs break-words text-muted-foreground">Select every agent that should receive this identical definition.</p><div className="grid gap-3 sm:grid-cols-2">{supportedHarnesses.map((harness) => <Label className="flex min-w-0 items-center gap-2 font-normal capitalize" key={harness}><Checkbox checked={draft.harnesses.includes(harness)} onCheckedChange={(checked) => toggleHarness(harness, checked === true)} /><span className="truncate" title={harness}>{harness}</span></Label>)}</div></fieldset>
          <Label className="flex items-center gap-2 font-normal"><Checkbox checked={!draft.disabled} onCheckedChange={(checked) => setDraft({ ...draft, disabled: checked !== true })} />Enabled</Label>
          {error && <Alert variant="destructive"><AlertDescription>{error}</AlertDescription></Alert>}
        </div>
        <DialogFooter><Button type="button" variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button><Button type="button" onClick={save}>{definition ? "Save changes" : "Add to pending changes"}</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
