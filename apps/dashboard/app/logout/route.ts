import { NextRequest, NextResponse } from "next/server";
import { auth } from "@/lib/auth";

export async function GET(request: NextRequest) {
  try {
    await auth.api.signOut({ headers: request.headers });
  } catch {
    // Always clear the browser's route state by returning to the login page.
  }
  const publicUrl = process.env.BETTER_AUTH_URL ?? request.nextUrl.origin;
  return NextResponse.redirect(new URL("/login", publicUrl));
}
