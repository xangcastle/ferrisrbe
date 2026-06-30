import { NavLink, Outlet } from 'react-router-dom';

const navClass = ({ isActive }: { isActive: boolean }) =>
  `block rounded-md px-3 py-2 text-sm font-medium ${
    isActive
      ? 'bg-gray-900 text-white'
      : 'text-gray-700 hover:bg-gray-100 hover:text-gray-900'
  }`;

export default function Layout() {
  return (
    <div className="flex min-h-screen flex-col">
      <header className="border-b bg-white shadow-sm">
        <div className="mx-auto flex h-16 max-w-7xl items-center px-4 sm:px-6 lg:px-8">
          <h1 className="text-xl font-bold tracking-tight text-gray-900">FerrisRBE BES</h1>
        </div>
      </header>
      <div className="mx-auto flex w-full max-w-7xl flex-1 items-start px-4 py-6 sm:px-6 lg:px-8">
        <aside className="w-48 shrink-0">
          <nav className="space-y-1">
            <NavLink to="/" className={navClass} end>
              Dashboard
            </NavLink>
            <NavLink to="/builds" className={navClass}>
              Builds
            </NavLink>
            <NavLink to="/targets" className={navClass}>
              Targets
            </NavLink>
            <NavLink to="/tests" className={navClass}>
              Tests
            </NavLink>
          </nav>
        </aside>
        <main className="min-w-0 flex-1 pl-6">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
