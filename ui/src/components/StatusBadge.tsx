interface StatusBadgeProps {
  status: string;
}

const statusClasses: Record<string, string> = {
  in_progress: 'bg-yellow-100 text-yellow-800 border-yellow-200',
  success: 'bg-green-100 text-green-800 border-green-200',
  failure: 'bg-red-100 text-red-800 border-red-200',
  cached: 'bg-blue-100 text-blue-800 border-blue-200',
};

const statusLabels: Record<string, string> = {
  in_progress: 'In Progress',
  success: 'Success',
  failure: 'Failure',
  cached: 'Cached',
};

export default function StatusBadge({ status }: StatusBadgeProps) {
  const normalized = status.toLowerCase();
  const classes = statusClasses[normalized] ?? 'bg-gray-100 text-gray-800 border-gray-200';
  const label = statusLabels[normalized] ?? status;
  return (
    <span
      className={`inline-flex items-center rounded-full border px-2.5 py-0.5 text-xs font-medium ${classes}`}
    >
      {label}
    </span>
  );
}
