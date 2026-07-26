import PageHeader from "@/components/PageHeader";
import Milestone from "@/components/Milestone";

export default function LibraryPage() {
  return (
    <>
      <PageHeader
        title="Library"
        description="Every artifact this app has produced, with checksums and retention."
      />
      <Milestone milestone="M2′">
        Manifest reading, SHA-256 verification and retention planning are
        implemented and tested in the engine; the library lists real artifacts
        once backups can produce them.
      </Milestone>
    </>
  );
}
