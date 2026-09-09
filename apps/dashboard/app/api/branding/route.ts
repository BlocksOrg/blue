import { NextResponse } from "next/server";
import { api } from "@/lib/api";
import type { Branding } from "@/lib/branding";

export async function GET() {
  try {
    return NextResponse.json(await api<Branding>("/branding"));
  } catch {
    return NextResponse.json(
      { error: "Branding settings are unavailable." },
      { status: 503 },
    );
  }
}
