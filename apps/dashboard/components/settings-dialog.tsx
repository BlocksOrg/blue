"use client";

import { useActionState, useEffect } from "react";
import {
  saveBranding,
  type BrandingState,
} from "@/app/actions";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs";
import { resolveBranding, type Branding } from "@/lib/branding";
import { applyFavicon } from "@/lib/branding-client";

const initialState: BrandingState = {};

export function SettingsDialog({
  open,
  onOpenChange,
  value,
  onSaved,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  value: Branding;
  onSaved: (value: Branding) => void;
}) {
  const [state, action, pending] = useActionState(saveBranding, initialState);

  useEffect(() => {
    if (!state.saved || !state.branding) return;
    applyFavicon(resolveBranding(state.branding).favicon_url);
    onSaved(state.branding);
    onOpenChange(false);
  }, [state, onOpenChange, onSaved]);

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!pending) onOpenChange(nextOpen);
      }}
    >
      <DialogContent
        className="max-h-[80svh] overflow-hidden sm:max-w-xl"
        showCloseButton={!pending}
      >
        <DialogHeader>
          <DialogTitle>Settings</DialogTitle>
          <DialogDescription>
            Customize deployment-wide settings for Blue.
          </DialogDescription>
        </DialogHeader>
        <form action={action} className="contents">
          <Tabs defaultValue="theming" className="min-h-0 overflow-y-auto">
            <TabsList variant="line" className="justify-start border-b">
              <TabsTrigger value="theming" className="px-3 py-2">
                Theming
              </TabsTrigger>
            </TabsList>
            <TabsContent value="theming" className="grid gap-4 pt-4">
              <div className="grid gap-2">
                <Label htmlFor="branding-logo-url">Logo URL</Label>
                <Input
                  id="branding-logo-url"
                  name="logo_url"
                  type="url"
                  inputMode="url"
                  maxLength={2048}
                  defaultValue={value.logo_url ?? ""}
                  placeholder="https://example.com/logo.svg"
                  disabled={pending}
                />
                <p className="text-xs text-muted-foreground">
                  Shown in the sidebar and above the logged-out sign-in form.
                  Leave blank to use the default Blue mark.
                </p>
              </div>
              <div className="grid gap-2">
                <Label htmlFor="branding-favicon-url">Favicon URL</Label>
                <Input
                  id="branding-favicon-url"
                  name="favicon_url"
                  type="url"
                  inputMode="url"
                  maxLength={2048}
                  defaultValue={value.favicon_url ?? ""}
                  placeholder="https://example.com/favicon.ico"
                  disabled={pending}
                />
                <p className="text-xs text-muted-foreground">
                  Used as the browser icon for every Blue page. Leave blank to
                  remove the custom favicon.
                </p>
              </div>
              {state.error && (
                <Alert variant="destructive">
                  <AlertDescription>{state.error}</AlertDescription>
                </Alert>
              )}
            </TabsContent>
          </Tabs>
          <DialogFooter>
            <DialogClose
              render={
                <Button type="button" variant="outline" disabled={pending} />
              }
            >
              Cancel
            </DialogClose>
            <Button type="submit" disabled={pending}>
              {pending ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
