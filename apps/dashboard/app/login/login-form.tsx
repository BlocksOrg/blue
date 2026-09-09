"use client";

import { FormEvent, useState } from "react";
import { authClient } from "../../lib/auth-client";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

export function LoginForm({ callbackURL }: { callbackURL: string }) {
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [email, setEmail] = useState("");
  const [passwordStep, setPasswordStep] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setPending(true);
    setError("");
    try {
      const form = new FormData(event.currentTarget);
      const submittedEmail = String(form.get("email") ?? email)
        .trim()
        .toLowerCase();
      if (!passwordStep) {
        const response = await fetch("/api/login-method", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ email: submittedEmail }),
          cache: "no-store",
          signal: AbortSignal.timeout(10_000),
        });
        if (!response.ok) throw new Error("Login method lookup failed");
        const method = (await response.json()) as {
          method: "password" | "oidc";
          providerId?: string;
          providerName?: string;
        };
        if (method.method === "oidc" && method.providerId) {
          const result = await authClient.signIn.social({
            provider: method.providerId,
            callbackURL,
            errorCallbackURL: "/login?error=Identity+provider+sign-in+failed",
            loginHint: submittedEmail,
          });
          if (result.error) {
            setError(
              `Unable to start ${method.providerName ?? "identity provider"} sign-in`,
            );
          }
          return;
        }
        setEmail(submittedEmail);
        setPasswordStep(true);
        return;
      }
      const result = await authClient.signIn.email({
        email,
        password: String(form.get("password")),
        callbackURL,
      });
      if (result.error) {
        setError("Invalid email or password");
        return;
      }
      window.location.assign(callbackURL);
    } catch {
      setError("Unable to continue sign-in. Please try again.");
    } finally {
      setPending(false);
    }
  }

  return (
    <form onSubmit={submit} className="grid gap-4">
      <div className="grid gap-2">
        <Label htmlFor="email">Email</Label>
        <Input
          id="email"
          name="email"
          type="email"
          autoComplete="email"
          required
          autoFocus
          value={email}
          onChange={(event) => setEmail(event.target.value)}
          readOnly={passwordStep}
        />
      </div>
      {passwordStep && (
        <div className="grid gap-2">
          <Label htmlFor="password">Password</Label>
          <Input
            id="password"
            name="password"
            type="password"
            autoComplete="current-password"
            required
            autoFocus
          />
        </div>
      )}
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
      <Button type="submit" size="lg" disabled={pending}>
        {pending ? "Continuing…" : passwordStep ? "Sign in" : "Continue"}
      </Button>
      {passwordStep && (
        <Button
          type="button"
          variant="ghost"
          disabled={pending}
          onClick={() => {
            setPasswordStep(false);
            setError("");
          }}
        >
          Use another email
        </Button>
      )}
    </form>
  );
}
