import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { getTests } from '../api/bes';
import type { TestSummary } from '../types';

function statusClass(status: string) {
  switch (status) {
    case 'PASSED':
      return 'bg-green-100 text-green-800';
    case 'FAILED':
    case 'TIMEOUT':
    case 'INCOMPLETE':
    case 'REMOTE_FAILURE':
    case 'FAILED_TO_BUILD':
    case 'TOOL_HALTED_BEFORE_TESTING':
      return 'bg-red-100 text-red-800';
    case 'FLAKY':
      return 'bg-yellow-100 text-yellow-800';
    default:
      return 'bg-gray-100 text-gray-800';
  }
}

export default function Tests() {
  const [tests, setTests] = useState<TestSummary[]>([]);
  const [filter, setFilter] = useState('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();

  useEffect(() => {
    async function load() {
      try {
        setLoading(true);
        const data = await getTests();
        setTests(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load tests');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, []);

  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    return tests.filter((t) => t.label.toLowerCase().includes(q));
  }, [tests, filter]);

  if (loading) return <p className="text-gray-600">Loading tests...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-4">
      <h2 className="text-2xl font-bold text-gray-900">Tests</h2>
      <p className="text-sm text-gray-500">
        Historial independiente por test. Cada fila muestra el último estado y el conteo de ejecuciones.
      </p>

      <input
        type="text"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="Filter by test label..."
        className="w-full rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none focus:ring-1 focus:ring-blue-500 sm:w-96"
      />

      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Label
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Latest Status
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Total Runs
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Passed / Failed / Cached / Flaky
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {filtered.length === 0 && (
              <tr>
                <td colSpan={4} className="px-4 py-6 text-center text-gray-500">
                  No tests found.
                </td>
              </tr>
            )}
            {filtered.map((test) => (
              <tr
                key={test.label}
                onClick={() => navigate(`/tests/${encodeURIComponent(test.label)}`)}
                className="cursor-pointer hover:bg-gray-50"
              >
                <td className="px-4 py-3 text-sm font-medium text-gray-900">
                  <div className="max-w-md truncate" title={test.label}>
                    {test.label}
                  </div>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm">
                  <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${statusClass(test.latest_status)}`}>
                    {test.latest_status}
                  </span>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">{test.total_runs}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  <span className="text-green-600">{test.passed_count}</span>
                  {' / '}
                  <span className="text-red-600">{test.failed_count}</span>
                  {' / '}
                  <span className="text-blue-600">{test.cached_count}</span>
                  {' / '}
                  <span className="text-yellow-600">{test.flaky_count}</span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
