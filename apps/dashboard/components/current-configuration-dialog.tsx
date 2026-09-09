"use client";

import { useState } from "react";
import { Check, Copy, Download } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

export type BlueConfigExport = {
  yaml: string;
  redactions: { path: string; environment_variable: string }[];
};

export function CurrentConfigurationDialog({
  open,
  onOpenChange,
  value,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  value: BlueConfigExport;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    await navigator.clipboard.writeText(value.yaml);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  }

  function download() {
    const url = URL.createObjectURL(
      new Blob([value.yaml], { type: "application/yaml" }),
    );
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = "blue.yaml";
    anchor.click();
    URL.revokeObjectURL(url);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[80svh] grid-rows-[auto_minmax(0,1fr)] overflow-hidden sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Current deployment configuration</DialogTitle>
          <DialogDescription>
            Review, copy, or download the configuration currently deployed by
            Blue.
          </DialogDescription>
        </DialogHeader>
        <div className="grid min-h-0 gap-3 overflow-hidden">
          {value.redactions.length > 0 && (
            <Alert>
              <AlertDescription>
                Literal secrets were replaced with environment references:{" "}
                {value.redactions
                  .map((item) => item.environment_variable)
                  .join(", ")}
                .
              </AlertDescription>
            </Alert>
          )}
          <div className="relative min-h-0 overflow-hidden">
            <div className="absolute right-3 top-3 z-10 flex items-center gap-1 rounded-lg border bg-popover/95 p-1 shadow-md backdrop-blur">
              <Button type="button" size="sm" variant="ghost" onClick={copy}>
                {copied ? <Check /> : <Copy />}
                {copied ? "Copied" : "Copy"}
              </Button>
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={download}
              >
                <Download />
                Download
              </Button>
            </div>
            <pre
              aria-label="Current deployable Blue configuration"
              className="size-full min-h-80 overflow-auto rounded-lg border bg-muted/20 p-4 pt-16 font-mono text-xs leading-relaxed text-foreground"
              tabIndex={0}
            >
              {value.yaml}
            </pre>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
