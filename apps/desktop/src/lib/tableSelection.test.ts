import { describe, expect, it } from "vitest";

import {
  keyOf,
  materialise,
  modesFromSelections,
  unlistedTables,
} from "./tableSelection";
import type { TableInfo, TableSelection } from "@/bindings";

/**
 * These four functions decide what a backup actually contains.
 *
 * Getting one wrong turns a 109-table backup into a 3-table one, and the
 * result still succeeds, still writes an artifact, and still reports green —
 * the loss only shows up when somebody needs the data back. Nothing else in
 * the frontend is worth testing this closely.
 */

function table(name: string, schema: string | null = null): TableInfo {
  return {
    name,
    schema,
    storage_engine: null,
    transactional: true,
    estimated_rows: null,
    data_bytes: null,
    index_bytes: null,
  };
}

describe("keyOf", () => {
  it("qualifies with the schema when there is one", () => {
    expect(keyOf(table("orders", "public"))).toBe("public.orders");
  });

  it("leaves an unqualified table alone", () => {
    // MySQL and MongoDB have no schema, and inventing one would name a table
    // the engine cannot find.
    expect(keyOf(table("orders"))).toBe("orders");
  });
});

describe("materialise", () => {
  const tables = [table("orders"), table("users"), table("audit_log")];

  it("covers every table, not just the ones clicked", () => {
    // The sparse map holds only what the user touched. Sending it as-is would
    // leave the rest unmentioned, and an unmentioned table means different
    // things to mysqldump and pg_dump.
    const out = materialise(tables, { orders: "schema_and_data" }, "schema_only");

    expect(out).toHaveLength(3);
    expect(out.map((s) => s.name).sort()).toEqual([
      "audit_log",
      "orders",
      "users",
    ]);
  });

  it("applies the default to untouched tables and the choice to touched ones", () => {
    const out = materialise(
      tables,
      { orders: "schema_and_data", audit_log: "exclude" },
      "schema_only",
    );
    const mode = (n: string) => out.find((s) => s.name === n)?.mode;

    expect(mode("orders")).toBe("schema_and_data");
    expect(mode("audit_log")).toBe("exclude");
    expect(mode("users")).toBe("schema_only");
  });

  it("uses schema-qualified names", () => {
    const out = materialise([table("orders", "public")], {}, "schema_only");
    expect(out.map((s) => s.name)).toEqual(["public.orders"]);
  });

  it("produces nothing from no tables", () => {
    expect(materialise([], { orders: "schema_and_data" }, "schema_only")).toEqual(
      [],
    );
  });
});

describe("modesFromSelections", () => {
  it("round-trips a materialised list", () => {
    const tables = [table("orders"), table("users")];
    const modes = { orders: "exclude" as const };
    const selections = materialise(tables, modes, "schema_only");

    expect(modesFromSelections(selections)).toEqual({
      orders: "exclude",
      users: "schema_only",
    });
  });
});

describe("unlistedTables", () => {
  const listed: TableSelection[] = [
    { name: "orders", mode: "schema_and_data", where_filter: null },
  ];

  it("reports what a set does not mention", () => {
    const out = unlistedTables([table("orders"), table("invoices")], listed);
    expect(out).toEqual(["invoices"]);
  });

  it("treats a bare saved name as the public schema", () => {
    // Mirrors `plan::expand_selections`. A set imported from a legacy
    // tables.conf holds bare names while PostgreSQL introspection qualifies
    // them; counting `public.orders` as unlisted would tell the user their
    // exclusion had been lost when it had not.
    const out = unlistedTables([table("orders", "public")], listed);
    expect(out).toEqual([]);
  });

  it("does not treat another schema as the same table", () => {
    const out = unlistedTables([table("orders", "archive")], listed);
    expect(out).toEqual(["archive.orders"]);
  });

  it("reports everything when the set is empty", () => {
    expect(unlistedTables([table("orders"), table("users")], [])).toEqual([
      "orders",
      "users",
    ]);
  });
});
