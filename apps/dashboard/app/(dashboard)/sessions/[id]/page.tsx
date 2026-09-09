import { api } from "../../../../lib/api";
import { downloadSession } from "../../../actions";
import Link from "next/link";
import { ArrowLeft, Download } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { SessionShareDialog, SessionSharingSummary } from "../session-sharing";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

type Artifact = {
  id: string;
  sha256: string;
  size_bytes: number;
  content_type: string;
  status: string;
  completed_at?: string;
  retention_expires_at?: string;
};
type Detail = {
  session: {
    id: string;
    user_id: string;
    harness: string;
    compatibility_profile: string;
    native_session_id: string;
    user_email: string;
    cwd?: string;
    updated_at: string;
    artifact_format: string;
    resumable: boolean;
    title?: string;
    summary?: string;
    sharing_mode: "private" | "workspace" | "selected";
    share_recipient_count: number;
  };
  sharing: { user_ids: string[] };
  artifacts: Artifact[];
};
export default async function SessionDetail({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  const [item, me] = await Promise.all([
    api<Detail>(`/session-uploads/${id}`),
    import("../../../../lib/api").then(({ requireIdentity }) => requireIdentity()),
  ]);
  return (
    <div className="flex flex-col gap-6">
      <Link href="/sessions" className="inline-flex w-fit items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeft className="size-4" /> Sessions
      </Link>
      <div className="flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight">{item.session.title ?? `${item.session.harness} session`}</h1>
        </div>
        <div className="flex shrink-0 flex-wrap gap-2">
          {item.session.user_id === me.id && (
            <SessionShareDialog
              sessionId={id}
              sessionLabel={item.session.title ?? `${item.session.harness} session`}
              trigger={<Button type="button" variant="outline">Share</Button>}
            />
          )}
          <form action={downloadSession}>
            <input type="hidden" name="session_id" value={id} />
            <Button type="submit"><Download /> Download {item.session.resumable ? "session bundle" : "raw data"}</Button>
          </form>
        </div>
      </div>
      <section className="border-y py-5">
        <h2 className="mb-4 text-sm font-medium">Session details</h2>
        <dl className="grid gap-5 sm:grid-cols-2 lg:grid-cols-6">
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">Native ID</dt>
            <dd className="mt-1 truncate font-mono text-xs" title={item.session.native_session_id}>{item.session.native_session_id}</dd>
          </div>
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">User</dt>
            <dd className="mt-1 truncate text-sm" title={item.session.user_email}>{item.session.user_email}</dd>
          </div>
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">Adapter profile</dt>
            <dd className="mt-1 truncate font-mono text-xs" title={item.session.compatibility_profile}>{item.session.compatibility_profile}</dd>
          </div>
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">Working directory</dt>
            <dd className="mt-1 truncate font-mono text-xs" title={item.session.cwd ?? "—"}>{item.session.cwd ?? "—"}</dd>
          </div>
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">Updated</dt>
            <dd className="mt-1 text-sm">{new Date(item.session.updated_at).toLocaleString()}</dd>
          </div>
          <div className="min-w-0">
            <dt className="text-xs text-muted-foreground">Shared with</dt>
            <dd className="mt-1">
              <SessionSharingSummary
                sessionId={id}
                mode={item.session.sharing_mode}
                recipientCount={item.session.share_recipient_count}
              />
            </dd>
          </div>
        </dl>
      </section>
      <section>
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-medium">Artifacts</h2>
          <span className="text-xs text-muted-foreground">{item.artifacts.length} total</span>
        </div>
        <div className="-mx-4 overflow-x-auto border-y sm:-mx-6 lg:-mx-8">
          <Table className="[&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_th:first-child]:pl-4 [&_th:last-child]:pr-4 sm:[&_td:first-child]:pl-6 sm:[&_td:last-child]:pr-6 sm:[&_th:first-child]:pl-6 sm:[&_th:last-child]:pr-6 lg:[&_td:first-child]:pl-8 lg:[&_td:last-child]:pr-8 lg:[&_th:first-child]:pl-8 lg:[&_th:last-child]:pr-8">
            <TableHeader className="bg-background/35 text-muted-foreground">
              <TableRow>
                <TableHead>Status</TableHead>
                <TableHead>Size</TableHead>
                <TableHead>SHA-256</TableHead>
                <TableHead>Retention</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {item.artifacts.map((a) => (
                <TableRow key={a.id} className="h-12 hover:bg-background/25">
                  <TableCell>
                    <Badge variant="outline">{a.status}</Badge>
                  </TableCell>
                  <TableCell>{a.size_bytes.toLocaleString()} B</TableCell>
                  <TableCell className="max-w-sm truncate font-mono text-xs text-muted-foreground">
                    {a.sha256}
                  </TableCell>
                  <TableCell>
                    {a.retention_expires_at
                      ? new Date(a.retention_expires_at).toLocaleString()
                      : "—"}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </section>
    </div>
  );
}
