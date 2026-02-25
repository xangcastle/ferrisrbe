# Sistema de Detección de Versión de Bazel

Este módulo implementa un sistema extensible para detectar la versión de Bazel desde requests gRPC y adaptar las respuestas del servidor según la versión de REAPI requerida.

## Arquitectura

```
┌─────────────────────────────────────────────────────────────────┐
│                    REQUEST gRPC (Bazel Client)                  │
└───────────────────────────┬─────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────┐ │
│  │ VersionDetector │───▶│ VersionContext  │───▶│   Handler   │ │
│  │  (gRPC Headers) │    │  (BazelVersion) │    │  (Registry) │ │
│  └─────────────────┘    └─────────────────┘    └─────────────┘ │
│                              ▲                                  │
│                              │                                  │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ VersionManager (combina detector + registry + handler)  │   │
│  └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  ServerCapabilities adaptadas según versión de Bazel            │
└─────────────────────────────────────────────────────────────────┘
```

## Componentes Principales

### 1. Traits (`traits.rs`)

Define los contratos fundamentales del sistema:

- **`BazelVersion`**: Representa una versión semántica (major.minor.patch)
- **`VersionDetector`**: Detecta versión desde requests gRPC
- **`BazelVersionHandler`**: Adapta respuestas según versión
- **`VersionContext`**: Contexto de versión para requests

### 2. Detectores (`detector.rs`)

Implementaciones de `VersionDetector`:

- **`GrpcMetadataDetector`**: Extrae versión de headers gRPC
  - `user-agent`: `"bazel/8.3.0"`
  - `x-bazel-version`: Header opcional
- **`CompositeDetector`**: Combina múltiples detectores
- **`DefaultVersionDetector`**: Valor por defecto configurable

### 3. Registry (`registry.rs`)

**`VersionRegistry`**: Mantiene un registro de handlers por rango de versiones.

```rust
let registry = VersionRegistry::with_defaults();
let handler = registry.get_handler(BazelVersion::new(8, 3, 0));
// Returns: Bazel8Handler
```

### 4. Handlers (`handlers/`)

Cada handler implementa adaptaciones específicas:

| Handler | Versión Bazel | REAPI | Características |
|---------|--------------|-------|-----------------|
| `Bazel7Handler` | 7.0.0 - 8.0.0 | v2.3/v2.4 | Transición, campos opcionales |
| `Bazel8Handler` | 8.0.0 - 9.0.0 | v2.4 | `deprecated_api_version` requerido |
| `Bazel9Handler` | 9.0.0+ | v2.4+ | Full v2.4, forward-compatible |

### 5. Manager (`mod.rs`)

**`VersionManager`**: API unificada que combina detector y registry.

```rust
let manager = VersionManager::new();
let (context, handler) = manager.detect_and_get_handler(&request);
```

## Flujo de Uso

### En CapabilitiesService

```rust
async fn get_capabilities(&self, request: Request<GetCapabilitiesRequest>) 
    -> Result<Response<ServerCapabilities>, Status> 
{
    // 1. Crear capabilities base
    let mut caps = self.create_base_capabilities();
    
    // 2. Detectar versión y obtener handler
    let (context, handler) = self.version_manager.detect_and_get_handler(&request);
    
    // 3. Adaptar capabilities según versión
    if let Some(version) = context.bazel_version {
        handler.adapt_capabilities(&mut caps, version);
    }
    
    Ok(Response::new(caps))
}
```

## Agregar Soporte para Nueva Versión

### Paso 1: Crear Handler

```rust
// src/version/handlers/v10_handler.rs
use crate::version::traits::{BazelVersion, BazelVersionHandler, ReapiField};
use crate::proto::build::bazel::remote::execution::v2::ServerCapabilities;

pub struct Bazel10Handler;

#[async_trait::async_trait]
impl BazelVersionHandler for Bazel10Handler {
    fn version_range(&self) -> (BazelVersion, Option<BazelVersion>) {
        // Soporta 10.0.0 en adelante
        (BazelVersion::new(10, 0, 0), None)
    }

    fn adapt_capabilities(&self, caps: &mut ServerCapabilities, version: BazelVersion) {
        // REAPI v2.5+ (ejemplo)
        caps.deprecated_api_version = Some(reapi_v2_0_semver());
        caps.low_api_version = Some(reapi_v2_0_semver());
        caps.high_api_version = Some(reapi_v2_5_semver());
        
        // Nuevas features de Bazel 10
        if let Some(ref mut cache_caps) = caps.cache_capabilities {
            cache_caps.supported_compressors = vec![1, 2, 3]; // identity, zstd, brotli
        }
    }

    fn requires_field(&self, field: ReapiField) -> bool {
        match field {
            ReapiField::DeprecatedApiVersion => true,
            ReapiField::LowApiVersion => true,
            ReapiField::HighApiVersion => true,
            // Nuevos campos en v2.5
            ReapiField::CacheCapabilitiesExtended => true,
            ReapiField::ZstdCompression => true,
            ReapiField::SymlinkStrategy => true,
        }
    }

    fn name(&self) -> &'static str {
        "Bazel10Handler"
    }
}
```

### Paso 2: Exportar

```rust
// src/version/handlers/mod.rs
pub mod v10_handler;
pub use v10_handler::Bazel10Handler;
```

### Paso 3: Registrar

```rust
// En VersionRegistry::register_default_handlers()
pub fn register_default_handlers(&mut self) {
    self.register(Arc::new(Bazel7Handler::new()));
    self.register(Arc::new(Bazel8Handler::new()));
    self.register(Arc::new(Bazel9Handler::new()));
    self.register(Arc::new(Bazel10Handler::new())); // NUEVO
}
```

### Paso 4: Crear Ejemplo de Prueba

```bash
mkdir -p examples/bazel-10.x
cp -r examples/bazel-9.x/* examples/bazel-10.x/
```

Actualizar:
- `MODULE.bazel`: Cambiar nombre del módulo
- `BUILD.bazel`: Actualizar mensajes de versión
- `.bazelrc`: Configurar para Bazel 10.x

## Testing

```bash
# Test unitarios del sistema de versiones
cargo test version

# Test específico de handlers
cargo test --lib version::handlers

# Test de detección
cargo test --lib version::detector
```

## Debugging

Habilitar logs de debug:

```bash
export RUST_LOG=debug
cargo run
```

Verás mensajes como:
```
🔍 Versión de Bazel detectada: 8.3.0
📋 Registrando handler 'Bazel8Handler' para rango [8.0.0, 9.0.0)
✅ Handler encontrado: 'Bazel8Handler'
Adaptando capabilities para Bazel 8.x (versión 8.3.0)
```

## Referencias

- [Bazel Remote Execution API](https://github.com/bazelbuild/remote-apis)
- [REAPI v2.4 Changes](https://github.com/bazelbuild/remote-apis/releases)
- `REAPI_COMPATIBILITY.md` - Documentación de compatibilidad
