import PageHeader from "@/components/PageHeader";
import Milestone from "@/components/Milestone";

export default function SyncPage() {
  return (
    <>
      <PageHeader
        title="Sync"
        description="Copy a selection of tables from one server to another in a single job."
      />
      <Milestone milestone="M4′">
        The end-to-end wizard builds on the backup and restore pipelines, so it
        lands once both are running.
      </Milestone>
    </>
  );
}
