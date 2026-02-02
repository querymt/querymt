/**
 * App.tsx - Main application router
 * UI Refactor Complete - All phases implemented
 * 
 * Route Structure:
 * / - HomePage (session picker or welcome screen)
 * /session/:sessionId - ChatView (main session chat interface)
 * 
 * AppShell provides:
 * - Header with home button, session chip, stats bar, model picker
 * - SessionSwitcher modal (Cmd+/)
 * - StatsDrawer (click stats bar)
 * - Outlet for child routes
 */

import { Routes, Route, Navigate } from 'react-router-dom';
import { AppShell } from './components/AppShell';
import { HomePage } from './components/HomePage';
import { ChatView } from './components/ChatView';

function App() {
  return (
    <Routes>
      <Route element={<AppShell />}>
        <Route path="/" element={<HomePage />} />
        <Route path="/session/:sessionId" element={<ChatView />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}

export default App;
