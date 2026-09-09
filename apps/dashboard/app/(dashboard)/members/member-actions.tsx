"use client";

import { useState } from "react";
import { Ellipsis } from "lucide-react";
import {
  cancelInvitation,
  deleteUser,
  resendInvitation,
  revokeUserSessions,
  updateUser,
} from "@/app/actions";
import {
  ConfirmationDialog,
  type ConfirmationAction,
} from "@/components/confirmation-dialog";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

function ActionItem({
  action,
  fields,
  children,
  disabled,
  destructive,
}: {
  action: (form: FormData) => void | Promise<void>;
  fields: Record<string, string>;
  children: React.ReactNode;
  disabled?: boolean;
  destructive?: boolean;
}) {
  return (
    <form action={action}>
      {Object.entries(fields).map(([name, value]) => <input key={name} type="hidden" name={name} value={value} />)}
      <DropdownMenuItem
        render={<button type="submit" disabled={disabled} className="w-full" />}
        variant={destructive ? "destructive" : "default"}
        disabled={disabled}
      >
        {children}
      </DropdownMenuItem>
    </form>
  );
}

function ConfirmationItem({
  children,
  disabled,
  destructive,
  onSelect,
}: {
  children: React.ReactNode;
  disabled?: boolean;
  destructive?: boolean;
  onSelect: () => void;
}) {
  return (
    <DropdownMenuItem
      render={<button type="button" disabled={disabled} className="w-full" onClick={onSelect} />}
      variant={destructive ? "destructive" : "default"}
      disabled={disabled}
    >
      {children}
    </DropdownMenuItem>
  );
}

export function MemberActions({ user, currentUserId }: {
  user: { id: string; email: string; role: string; status: string; protected: boolean; managed: boolean };
  currentUserId: string;
}) {
  const [confirmation, setConfirmation] = useState<ConfirmationAction>();
  const locked = user.id === currentUserId || user.protected || user.managed;
  const removed = user.status === "removed";
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger render={<Button variant="ghost" size="icon-sm" aria-label={`Actions for ${user.email}`} />}><Ellipsis /></DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-48">
          {!removed && <ActionItem action={updateUser} fields={{ user_id: user.id, field: "role", value: user.role === "admin" ? "member" : "admin" }} disabled={locked}>Make {user.role === "admin" ? "member" : "admin"}</ActionItem>}
          {!removed && <ConfirmationItem disabled={locked} destructive={user.status === "active"} onSelect={() => setConfirmation({ action: updateUser, fields: { user_id: user.id, field: "status", value: user.status === "active" ? "suspended" : "active" }, title: `${user.status === "active" ? "Suspend" : "Reactivate"} member?`, description: user.status === "active" ? `${user.email} will lose access until an administrator reactivates the account.` : `${user.email} will regain access to the organization.`, confirmLabel: user.status === "active" ? "Suspend member" : "Reactivate member", destructive: user.status === "active" })}>{user.status === "active" ? "Suspend" : "Reactivate"}</ConfirmationItem>}
          {!removed && <ConfirmationItem destructive onSelect={() => setConfirmation({ action: revokeUserSessions, fields: { user_id: user.id }, title: "Revoke all sessions?", description: `${user.email} will be signed out of every browser and CLI session and must authenticate again.`, confirmLabel: "Revoke sessions", destructive: true })}>Revoke sessions</ConfirmationItem>}
          <DropdownMenuSeparator />
          <ConfirmationItem disabled={removed || locked} destructive onSelect={() => setConfirmation({ action: deleteUser, fields: { user_id: user.id }, title: "Delete login access?", description: `${user.email} will no longer be able to sign in. Historical governance and session records will be retained.`, confirmLabel: "Delete access", destructive: true })}>Delete access</ConfirmationItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <ConfirmationDialog confirmation={confirmation} open={Boolean(confirmation)} onOpenChange={(open) => { if (!open) setConfirmation(undefined); }} />
    </>
  );
}

export function InvitationActions({ invitation }: {
  invitation: { id: string; email: string; status: string };
}) {
  const [confirmation, setConfirmation] = useState<ConfirmationAction>();
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger render={<Button variant="ghost" size="icon-sm" aria-label={`Actions for ${invitation.email}`} />}><Ellipsis /></DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-36">
          {(invitation.status === "pending" || invitation.status === "expired") && <ActionItem action={resendInvitation} fields={{ invitation_id: invitation.id }}>Resend</ActionItem>}
          {invitation.status === "pending" && <ConfirmationItem destructive onSelect={() => setConfirmation({ action: cancelInvitation, fields: { invitation_id: invitation.id }, title: "Cancel invitation?", description: `${invitation.email} will no longer be able to accept this invitation.`, confirmLabel: "Cancel invitation", destructive: true })}>Cancel</ConfirmationItem>}
        </DropdownMenuContent>
      </DropdownMenu>
      <ConfirmationDialog confirmation={confirmation} open={Boolean(confirmation)} onOpenChange={(open) => { if (!open) setConfirmation(undefined); }} />
    </>
  );
}
