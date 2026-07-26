import PageHeader from "@/components/PageHeader";
import Milestone from "@/components/Milestone";

export default function RestorePage() {
  return (
    <>
      <PageHeader
        title="Restore"
        description="Restore an artifact into a new or existing database, with verification."
      />
      <Milestone milestone="M2′ (MySQL) and M3′ (PostgreSQL)">
        Target naming strategies, typed confirmation for destructive restores,
        and the selective/parallel restore rules are implemented and tested;
        executing them waits on connectivity.
      </Milestone>
    </>
  );
}
