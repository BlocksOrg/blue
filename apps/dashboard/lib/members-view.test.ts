import assert from "node:assert/strict";
import test from "node:test";
import { resolveMemberFilters, resolveMembersTab } from "./members-view.ts";

test("managed organizations can open identity but not invitations", () => {
  assert.equal(resolveMembersTab("identity", true, false), "identity");
  assert.equal(resolveMembersTab("invited", true, false), "members");
});

test("unmanaged organizations can open invitations but not identity", () => {
  assert.equal(resolveMembersTab("invited", false, true), "invited");
  assert.equal(resolveMembersTab("identity", false, true), "members");
});

test("members remains the default tab", () => {
  assert.equal(resolveMembersTab("", true, false), "members");
  assert.equal(resolveMembersTab("unknown", false, true), "members");
});

test("OIDC without SCIM still hides invitations", () => {
  assert.equal(resolveMembersTab("invited", false, false), "members");
});

test("member filters keep supported values", () => {
  assert.deepEqual(
    resolveMemberFilters({
      q: "  ada@example.com  ",
      role: "admin",
      status: "suspended",
      provisioning_source: "scim",
    }),
    {
      q: "ada@example.com",
      role: "admin",
      status: "suspended",
      provisioning_source: "scim",
    },
  );
});

test("member filters drop values the Control API would reject", () => {
  assert.deepEqual(
    resolveMemberFilters({
      q: "",
      role: "owner",
      status: "disabled",
      provisioning_source: "sso",
    }),
    { q: "", role: "", status: "", provisioning_source: "" },
  );
});

test("member search is clamped to the length the Control API accepts", () => {
  assert.equal(
    resolveMemberFilters({
      q: "a".repeat(500),
      role: "",
      status: "",
      provisioning_source: "",
    }).q.length,
    200,
  );
});
