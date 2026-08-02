import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";

import Sidebar from "@/components/Sidebar";
import ProfilesPage from "@/pages/ProfilesPage";
import SshPage from "@/pages/SshPage";
import JobsPage from "@/pages/JobsPage";
import JobDetailPage from "@/pages/JobDetailPage";
import BackupPage from "@/pages/BackupPage";
import RestorePage from "@/pages/RestorePage";
import SyncPage from "@/pages/SyncPage";
import TableSetsPage from "@/pages/TableSetsPage";
import SchedulesPage from "@/pages/SchedulesPage";
import MaskingPage from "@/pages/MaskingPage";
import LibraryPage from "@/pages/LibraryPage";
import DestinationsPage from "@/pages/DestinationsPage";
import SettingsPage from "@/pages/SettingsPage";
import { events } from "@/bindings";
import { useProgressStore } from "@/lib/jobProgress";

export default function App() {
  useTrayNavigation();
  useJobProgressFeed();

  return (
    <div className="flex h-full">
      <Sidebar />
      <main className="flex-1 overflow-y-auto">
        <Routes>
          <Route path="/" element={<Navigate to="/profiles" replace />} />
          <Route path="/profiles" element={<ProfilesPage />} />
          <Route path="/ssh" element={<SshPage />} />
          <Route path="/backup" element={<BackupPage />} />
          <Route path="/restore" element={<RestorePage />} />
          <Route path="/sync" element={<SyncPage />} />
          <Route path="/table-sets" element={<TableSetsPage />} />
          <Route path="/schedules" element={<SchedulesPage />} />
          <Route path="/masking" element={<MaskingPage />} />
          <Route path="/library" element={<LibraryPage />} />
          <Route path="/offsite" element={<DestinationsPage />} />
          <Route path="/jobs" element={<JobsPage />} />
          <Route path="/jobs/:jobId" element={<JobDetailPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<Navigate to="/profiles" replace />} />
        </Routes>
      </main>
    </div>
  );
}

/**
 * Collect job progress for the whole app, not just the Jobs page.
 *
 * Subscribed here because the events keep arriving while the user is on some
 * other page: starting a backup and then going to look at a connection should
 * not mean coming back to a progress bar that missed the first two minutes.
 */
function useJobProgressFeed() {
  const queryClient = useQueryClient();
  const record = useProgressStore((s) => s.record);
  const forget = useProgressStore((s) => s.forget);
  const noteFinished = useProgressStore((s) => s.noteFinished);

  useEffect(() => {
    const progress = events.jobProgress.listen((e) => record(e.payload));
    const finished = events.jobFinished.listen((e) => {
      // Kept for the jobs that leave no history row — an off-site push — whose
      // only record that they ended is this event.
      noteFinished(e.payload.job_id, e.payload.outcome);
      // The row's own outcome takes over from here, and keeping the last
      // sample would leave a 98% bar next to a green "success".
      forget(e.payload.job_id);
      void queryClient.invalidateQueries({ queryKey: ["jobs"] });
      void queryClient.invalidateQueries({ queryKey: ["active-jobs"] });
    });

    return () => {
      void progress.then((unlisten) => unlisten());
      void finished.then((unlisten) => unlisten());
    };
  }, [queryClient, record, forget, noteFinished]);
}

/**
 * Let the tray menu open a specific page.
 *
 * Routed here rather than by changing the window URL: a reload would tear down
 * any live job progress the user is watching.
 */
function useTrayNavigation() {
  const navigate = useNavigate();

  useEffect(() => {
    const unlisten = events.navigateTo.listen((event) => {
      navigate(event.payload);
    });
    return () => {
      void unlisten.then((f) => f());
    };
  }, [navigate]);
}
