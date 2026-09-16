"use client";

import { useActionState, useState } from "react";
import { Plus } from "lucide-react";
import { createUser, type InvitationLinkState } from "@/app/actions";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { InvitationLinkResult } from "./invitation-link-result";

const initialState: InvitationLinkState = {};

export function InviteMemberDialog() {
  const [open, setOpen] = useState(false);
  const [generation, setGeneration] = useState(0);
  return (
    <Dialog open={open} onOpenChange={(next) => { setOpen(next); if (next) setGeneration((value) => value + 1); }}>
      <DialogTrigger render={<Button />}><Plus /> Invite member</DialogTrigger>
      {open && <InviteMemberDialogContent key={generation} />}
    </Dialog>
  );
}

function InviteMemberDialogContent() {
  const [state, action, pending] = useActionState(createUser, initialState);

  return (
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{state.invitationUrl ? "Share invitation link" : "Create an invitation"}</DialogTitle>
          <DialogDescription>{state.invitationUrl ? "Copy this link now; it is not available from the invitation list later." : "Create a pinned-email link to share with the new member."}</DialogDescription>
        </DialogHeader>
        {state.invitationUrl && state.email ? (
          <InvitationLinkResult email={state.email} url={state.invitationUrl} />
        ) : (
          <form action={action} className="grid gap-5">
            <div className="grid gap-2"><Label htmlFor="invite-email">Email</Label><Input id="invite-email" name="email" type="email" required autoFocus /></div>
            <div className="grid gap-2"><Label>Role</Label><Select name="role" defaultValue="member"><SelectTrigger className="w-full"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="member">Member</SelectItem><SelectItem value="admin">Admin</SelectItem></SelectContent></Select></div>
            {state.error && <p role="alert" className="text-sm text-destructive">{state.error}</p>}
            <DialogFooter><Button type="submit" disabled={pending}>{pending ? "Creating…" : "Create invitation"}</Button></DialogFooter>
          </form>
        )}
      </DialogContent>
  );
}
