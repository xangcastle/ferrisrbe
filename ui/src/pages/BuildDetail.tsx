import { useEffect, useState } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getBuild, getEvents, getBuildTargets } from '../api/bes';
import StatusBadge from '../components/StatusBadge';
import type { BuildSummary, BuildEvent, TargetExecution } from '../types';

function formatTime(iso?: string) {
  if (!iso) return '-';
  return new Date(iso).toLocaleString();
}

function formatDuration(ms?: number) {
  if (ms === undefined || ms === null) return '-';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function EventItem({ event }: { event: BuildEvent }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="border-b last:border-b-0">
      <button
        onClick={() => setOpen(!open)}
        className="flex w-full items-center justify-between px-4 py-2 text-left hover:bg-gray-50"
      >
        <span className="font-medium text-gray-900">{event.kind}</span>
        <span className="text-xs text-gray-500">
          {event.timestamp ? new Date(event.timestamp).toLocaleString() : ''}
        </span>
      </button>
      {open && (
        <div className="bg-gray-50 px-4 py-3">
          <pre className="overflow-x-auto rounded-md bg-gray-100 p-3 text-xs text-gray-800">
            {JSON.stringify(event.payload, null, 2)}
          </pre>
        </div>
      )}
    </div>
  );
}

export default function BuildDetail() {
  const { id } = useParams<{ id: string }>();
  const [build, setBuild] = useState<BuildSummary | null>(null);
  const [events, setEvents] = useState<BuildEvent[]>([]);
  const [targets, setTargets] = useState<TargetExecution[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    async function load() {
      try {
        setLoading(true);
        const [buildData, eventsData, targetsData] = await Promise.all([
          getBuild(id!),
          getEvents(id!),
          getBuildTargets(id!),
        ]);
        setBuild(buildData);
        setEvents(eventsData);
        setTargets(targetsData);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load build');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, [id]);

  if (loading) return <p className="text-gray-600">Loading build...</p>;
  if (error) return <p className="text-red-600">{error}</p>;
  if (!build) return <p className="text-gray-600">Build not found.</p>;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h2 className="text-2xl font-bold text-gray-900">Build Detail</h2>
        <div className="flex gap-2">
          <Link
            to={`/targets`}
            className="rounded-md bg-white px-3 py-2 text-sm font-medium text-gray-700 ring-1 ring-gray-300 hover:bg-gray-50"
          >
            All Targets
          </Link>
          <Link
            to={`/misses/${encodeURIComponent(build.invocation_id)}`}
            className="rounded-md bg-blue-600 px-3 py-2 text-sm font-medium text-white hover:bg-blue-700"
          >
            View Misses
          </Link>
        </div>
      </div>

      <div className="rounded-lg border bg-white p-5 shadow-sm">
        <div className="mb-4 flex items-center gap-3">
          <StatusBadge status={build.status} />
          <span className="text-lg font-semibold text-gray-900">{build.command}</span>
        </div>
        <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <div>
            <dt className="text-sm font-medium text-gray-500">Invocation ID</dt>
            <dd className="mt-1 text-sm text-gray-900">{build.invocation_id}</dd>
          </div>
          <div>
            <dt className="text-sm font-medium text-gray-500">Workspace</dt>
            <dd className="mt-1 text-sm text-gray-900">{build.workspace}</dd>
          </div>
          <div>
            <dt className="text-sm font-medium text-gray-500">Duration</dt>
            <dd className="mt-1 text-sm text-gray-900">{formatDuration(build.duration_ms)}</dd>
          </div>
          <div>
            <dt className="text-sm font-medium text-gray-500">Start Time</dt>
            <dd className="mt-1 text-sm text-gray-900">{formatTime(build.start_time)}</dd>
          </div>
          <div>
            <dt className="text-sm font-medium text-gray-500">End Time</dt>
            <dd className="mt-1 text-sm text-gray-900">{formatTime(build.end_time)}</dd>
          </div>
          <div>
            <dt className="text-sm font-medium text-gray-500">Actions</dt>
            <dd className="mt-1 text-sm text-gray-900">
              {build.total_actions} total · {build.cached_actions} cached ·{' '}
              {build.remote_cache_hits} remote hits · {build.failed_actions} failed
            </dd>
          </div>
        </dl>
      </div>

      {build.errors.length > 0 && (
        <div className="rounded-lg border border-red-200 bg-red-50 p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-red-900">Errors</h3>
          <ul className="mt-2 list-disc space-y-1 pl-5">
            {build.errors.map((err, i) => (
              <li key={i} className="text-sm text-red-800">
                {err}
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="rounded-lg border bg-white shadow-sm">
        <div className="border-b px-4 py-3">
          <h3 className="text-lg font-semibold text-gray-900">Targets ({targets.length})</h3>
        </div>
        {targets.length === 0 ? (
          <p className="px-4 py-6 text-center text-gray-500">
            No target data available. Use{' '}
            <code className="rounded bg-gray-100 px-1 py-0.5 text-xs">--build_event_publish_all_actions=true</code>{' '}
            to capture more action details.
          </p>
        ) : (
          <div className="max-h-96 overflow-auto">
            <table className="min-w-full divide-y divide-gray-200">
              <thead className="bg-gray-50 sticky top-0">
                <tr>
                  <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Label
                  </th>
                  <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Kind
                  </th>
                  <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Status
                  </th>
                  <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Tags
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-200">
                {targets.map((target) => (
                  <tr key={target.label} className="hover:bg-gray-50">
                    <td className="px-4 py-3 text-sm font-medium text-gray-900">
                      <Link
                        to={`/targets/${encodeURIComponent(target.label)}`}
                        className="text-blue-600 hover:text-blue-800"
                      >
                        <div className="max-w-md truncate" title={target.label}>
                          {target.label}
                        </div>
                      </Link>
                    </td>
                    <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                      {target.target_kind}
                    </td>
                    <td className="whitespace-nowrap px-4 py-3 text-sm">
                      <StatusBadge status={target.status} />
                    </td>
                    <td className="px-4 py-3 text-sm text-gray-700">
                      <div className="flex flex-wrap gap-1">
                        {target.tags.map((tag) => (
                          <span key={tag} className="rounded-md bg-gray-100 px-2 py-0.5 text-xs text-gray-600">
                            {tag}
                          </span>
                        ))}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="rounded-lg border bg-white shadow-sm">
        <div className="border-b px-4 py-3">
          <h3 className="text-lg font-semibold text-gray-900">Events</h3>
        </div>
        <div>
          {events.length === 0 && (
            <p className="px-4 py-6 text-center text-gray-500">No events recorded.</p>
          )}
          {events.map((event, idx) => (
            <EventItem key={event.id ?? idx} event={event} />
          ))}
        </div>
      </div>
    </div>
  );
}
