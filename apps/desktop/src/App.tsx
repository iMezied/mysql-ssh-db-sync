import { Navigate, Route, Routes } from "react-router-dom";

import Sidebar from "@/components/Sidebar";
import ProfilesPage from "@/pages/ProfilesPage";
import JobsPage from "@/pages/JobsPage";
import BackupPage from "@/pages/BackupPage";
import RestorePage from "@/pages/RestorePage";
import SyncPage from "@/pages/SyncPage";
import LibraryPage from "@/pages/LibraryPage";
import SettingsPage from "@/pages/SettingsPage";

export default function App() {
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
          <Route path="/library" element={<LibraryPage />} />
          <Route path="/jobs" element={<JobsPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<Navigate to="/profiles" replace />} />
        </Routes>
      </main>
    </div>
  );
}
