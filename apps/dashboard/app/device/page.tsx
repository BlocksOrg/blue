import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

export default function DevicePage() {
  return (
    <main className="grid min-h-svh place-items-center bg-background p-4">
      <Card className="w-full max-w-sm rounded-lg border-0 bg-card/50 shadow-none ring-0">
        <CardHeader>
          <CardTitle className="text-xl">Authorize the CLI</CardTitle>
          <CardDescription>Start a temporary authorization from the terminal.</CardDescription>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            Run <code className="font-mono">blue login</code>. The CLI will
            open the confirmation page automatically; no code entry is
            required.
          </p>
        </CardContent>
      </Card>
    </main>
  );
}
