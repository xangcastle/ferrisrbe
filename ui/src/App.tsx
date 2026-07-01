import { Routes, Route } from 'react-router-dom';
import Layout from './components/Layout';
import Dashboard from './pages/Dashboard';
import Builds from './pages/Builds';
import BuildDetail from './pages/BuildDetail';
import Misses from './pages/Misses';
import Targets from './pages/Targets';
import TargetDetail from './pages/TargetDetail';
import Tests from './pages/Tests';
import TestDetail from './pages/TestDetail';

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Layout />}>
        <Route index element={<Dashboard />} />
        <Route path="builds" element={<Builds />} />
        <Route path="builds/:id" element={<BuildDetail />} />
        <Route path="misses/:id" element={<Misses />} />
        <Route path="targets" element={<Targets />} />
        <Route path="targets/:label" element={<TargetDetail />} />
        <Route path="tests" element={<Tests />} />
        <Route path="tests/:label" element={<TestDetail />} />
      </Route>
    </Routes>
  );
}
