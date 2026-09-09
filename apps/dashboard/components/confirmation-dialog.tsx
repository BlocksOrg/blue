"use client";

import { useActionState } from "react";
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

export type ConfirmationAction = {
  action: (formData: FormData) => void | Promise<void>;
  fields: Record<string, string>;
  title: string;
  description: string;
  confirmLabel: string;
  destructive?: boolean;
};

export function ConfirmationDialog({
  confirmation,
  open,
  onOpenChange,
}: {
  confirmation?: ConfirmationAction;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [, submitAction, pending] = useActionState(
    async (submission: number, formData: FormData) => {
      if (!confirmation) return submission;
      await confirmation.action(formData);
      onOpenChange(false);
      return submission + 1;
    },
    0,
  );

  if (!confirmation) return null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton={!pending}>
        <DialogHeader>
          <DialogTitle>{confirmation.title}</DialogTitle>
          <DialogDescription>{confirmation.description}</DialogDescription>
        </DialogHeader>
        <form action={submitAction} className="contents">
          {Object.entries(confirmation.fields).map(([name, value]) => (
            <input key={name} type="hidden" name={name} value={value} />
          ))}
          <DialogFooter>
            <DialogClose
              render={<Button type="button" variant="outline" disabled={pending} />}
            >
              Cancel
            </DialogClose>
            <Button
              type="submit"
              variant={confirmation.destructive ? "destructive" : "default"}
              disabled={pending}
            >
              {pending ? "Working…" : confirmation.confirmLabel}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
