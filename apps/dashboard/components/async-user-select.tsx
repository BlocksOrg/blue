"use client";

import { useEffect, useId, useRef, useState } from "react";
import { Check, ChevronDown, LoaderCircle, Search } from "lucide-react";
import {
  searchFilterUsers,
  type FilterUserOption,
  type FilterUserSource,
} from "@/app/actions";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

export function AsyncUserSelect({
  id,
  name,
  source,
  value,
  initialUsers,
}: {
  id: string;
  name: string;
  source: FilterUserSource;
  value: string;
  initialUsers: FilterUserOption[];
}) {
  const listId = useId();
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState(value);
  const [selectedEmail, setSelectedEmail] = useState(
    initialUsers.find((user) => user.id === value)?.email ?? "",
  );
  const [committed, setCommitted] = useState(value);
  const [query, setQuery] = useState("");
  const [users, setUsers] = useState(initialUsers);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [highlighted, setHighlighted] = useState(0);

  // Navigation (removing a filter chip, say) re-renders the page without
  // remounting, so the committed URL value has to win over stale local state.
  if (committed !== value) {
    setCommitted(value);
    setSelected(value);
    setSelectedEmail(initialUsers.find((user) => user.id === value)?.email ?? "");
  }

  useEffect(() => {
    function dismiss(event: MouseEvent) {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", dismiss);
    return () => document.removeEventListener("mousedown", dismiss);
  }, []);

  useEffect(() => {
    if (!open) return;
    let current = true;
    setPending(true);
    setError("");
    const timeout = window.setTimeout(async () => {
      const result = await searchFilterUsers(source, query, selected || undefined);
      if (!current) return;
      setUsers(result.users);
      setError(result.error ?? "");
      setPending(false);
      setHighlighted(0);
    }, query.trim() ? 250 : 0);
    return () => {
      current = false;
      window.clearTimeout(timeout);
    };
  }, [open, query, selected, source]);

  function close() {
    setOpen(false);
    trigger.current?.focus();
  }

  function choose(user?: FilterUserOption) {
    setSelected(user?.id ?? "");
    setSelectedEmail(user?.email ?? "");
    setQuery("");
    close();
  }

  const options: Array<FilterUserOption | undefined> = [undefined, ...users];

  return (
    <div ref={root} className="relative min-w-0">
      <input type="hidden" name={name} value={selected || "all"} />
      <button
        id={id}
        ref={trigger}
        type="button"
        className="flex h-8 w-full min-w-0 items-center justify-between gap-1.5 overflow-hidden rounded-lg border border-input bg-transparent py-2 pr-2 pl-2.5 text-sm outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        title={selectedEmail || "All users"}
        onClick={() => setOpen((current) => !current)}
      >
        <span className="min-w-0 flex-1 truncate text-left">{selectedEmail || "All users"}</span>
        <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
      </button>
      {open && (
        <div className="absolute z-50 mt-1 w-full min-w-0 max-w-[calc(100vw-3rem)] overflow-hidden rounded-lg bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10">
          <div className="relative border-b p-1.5">
            <Search className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              autoFocus
              value={query}
              maxLength={200}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") close();
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setHighlighted((current) => Math.min(options.length - 1, current + 1));
                }
                if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setHighlighted((current) => Math.max(0, current - 1));
                }
                if (event.key === "Enter") {
                  event.preventDefault();
                  choose(options[highlighted]);
                }
              }}
              className="pr-8 pl-8"
              placeholder="Search by email"
              role="combobox"
              aria-controls={listId}
              aria-expanded="true"
              aria-activedescendant={`${listId}-${highlighted}`}
            />
            {pending && <LoaderCircle className="absolute top-1/2 right-3 size-4 -translate-y-1/2 animate-spin text-muted-foreground" aria-label="Loading users" />}
          </div>
          <div id={listId} role="listbox" aria-busy={pending} aria-live="polite" className="max-h-60 overflow-y-auto p-1">
            <button
              id={`${listId}-0`}
              type="button"
              role="option"
              aria-selected={!selected}
              className={cn("flex w-full min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent", highlighted === 0 && "bg-accent")}
              onMouseEnter={() => setHighlighted(0)}
              onClick={() => choose()}
            >
              <span className="min-w-0 flex-1 truncate">All users</span>{!selected && <Check className="size-4 shrink-0" />}
            </button>
            {!pending && !error && users.length === 0 && <p className="px-2 py-3 text-center text-sm text-muted-foreground">No users found.</p>}
            {error && <p className="break-words px-2 py-3 text-sm text-destructive" role="alert">{error}</p>}
            {users.map((user, index) => (
              <button
                id={`${listId}-${index + 1}`}
                key={user.id}
                type="button"
                role="option"
                aria-selected={selected === user.id}
                title={user.email}
                className={cn("flex w-full min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent", highlighted === index + 1 && "bg-accent")}
                onMouseEnter={() => setHighlighted(index + 1)}
                onClick={() => choose(user)}
              >
                <span className="min-w-0 flex-1 truncate">{user.email}</span>{selected === user.id && <Check className="size-4 shrink-0" />}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
