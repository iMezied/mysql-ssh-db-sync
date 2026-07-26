import PageHeader from "@/components/PageHeader";
import Milestone from "@/components/Milestone";

export default function BackupPage() {
  return (
    <>
      <PageHeader
        title="Backup"
        description="Dump a source database with per-table control over schema and data."
      />
      <Milestone milestone="M2′ (MySQL) and M3′ (PostgreSQL)">
        Running a backup needs the SSH tunnel and table introspection landing in
        M1′. The options model, DEFINER filtering, manifest format and
        validation rules are already implemented and unit-tested in the engine.
      </Milestone>
    </>
  );
}
