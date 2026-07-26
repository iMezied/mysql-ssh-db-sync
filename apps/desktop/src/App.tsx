import { useEffect } from "react";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";

import Sidebar from "@/components/Sidebar";
import ProfilesPage from "@/pages/ProfilesPage";
import JobsPage from "@/pages/JobsPage";
import BackupPage from "@/pages/BackupPage";
import RestorePage from "@/pages/RestorePage";
import SyncPage from "@/pages/SyncPage";
import SchedulesPage from "@/pages/SchedulesPage";
import MaskingPage from "@/pages/MaskingPage";
import LibraryPage from "@/pages/LibraryPage";
import SettingsPage from "@/pages/SettingsPage";
import { events } from "@/bindings";

export default function App() {
  useTrayNavigation();

  return (
    <div className="flex h-full">
      <Sidebar />
      <main className="flex-1 overflow-y-auto">
        <Routes>
          <Route path="/" element={<Navigate to="/profiles" replace />} />
          <Route path="/profiles" element={<ProfilesPage />} />
          <Route path="/backup" element={<BackupPage />} />
          <Route path="/restore" element={<RestorePage />} />
          <Route path="/sync" element={<SyncPage />} />
          <Route path="/schedules" element={<SchedulesPage />} />
          <Route path="/masking" element={<MaskingPage />} />
          <Route path="/library" element={<LibraryPage />} />
          <Route path="/jobs" element={<JobsPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<Navigate to="/profiles" replace />} />
        </Routes>
      </main>
    </div>
  );
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
