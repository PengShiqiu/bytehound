use {
    reqwest::{
        header::{
            CONTENT_TYPE
        },
        StatusCode
    },
    serde_derive::{
        Deserialize
    },
    std::{
        ffi::{
            OsString
        },
        path::{
            Path,
            PathBuf
        },
        sync::{
            atomic::{
                AtomicUsize,
                Ordering
            }
        },
        thread,
        time::{
            Duration,
            Instant
        }
    },
    crate::{
        utils::*
    }
};

fn repository_root() -> PathBuf {
    Path::new( env!( "CARGO_MANIFEST_DIR" ) ).join( ".." ).canonicalize().unwrap()
}

fn preload_path() -> PathBuf {
    let path = if let Ok( path ) = std::env::var( "MEMORY_PROFILER_PRELOAD_PATH" ) {
        repository_root().join( "target" ).join( path ).join( "libmemory_profiler.so" )
    } else {
        repository_root().join( "target" ).join( "x86_64-unknown-linux-gnu" ).join( "release" ).join( "libmemory_profiler.so" )
    };

    assert!( path.exists(), "{:?} doesn't exist", path );
    path
}

fn cli_path() -> PathBuf {
    repository_root().join( "target" ).join( "x86_64-unknown-linux-gnu" ).join( "release" ).join( "memory-profiler-cli" )
}

#[derive(Deserialize)]
struct ResponseMetadata {
    pub id: String,
    pub executable: String,
    pub architecture: String
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Debug, Hash)]
#[serde(transparent)]
pub struct Secs( u64 );

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Debug, Hash)]
#[serde(transparent)]
pub struct FractNanos( u32 );

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Debug)]
pub struct Timeval {
    pub secs: Secs,
    pub fract_nsecs: FractNanos
}

#[derive(PartialEq, Deserialize, Debug)]
pub struct Deallocation {
    pub timestamp: Timeval,
    pub thread: u32
}

#[derive(PartialEq, Deserialize, Debug)]
pub struct Frame {
    pub address: u64,
    pub address_s: String,
    pub count: u64,
    pub library: Option< String >,
    pub function: Option< String >,
    pub raw_function: Option< String >,
    pub source: Option< String >,
    pub line: Option< u32 >,
    pub column: Option< u32 >,
    pub is_inline: bool
}

#[derive(PartialEq, Deserialize, Debug)]
pub struct Allocation {
    pub address: u64,
    pub address_s: String,
    pub timestamp: Timeval,
    pub timestamp_relative: Timeval,
    pub timestamp_relative_p: f32,
    pub thread: u32,
    pub size: u64,
    pub backtrace_id: u32,
    pub deallocation: Option< Deallocation >,
    pub backtrace: Vec< Frame >,
    pub is_mmaped: bool,
    pub in_main_arena: bool,
    pub extra_space: u32
}

#[derive(Deserialize, Debug)]
struct ResponseAllocations {
    pub allocations: Vec< Allocation >,
    pub total_count: u64
}

struct Analysis {
    response: ResponseAllocations
}

fn is_from_source( alloc: &Allocation, expected: &str ) -> bool {
    alloc.backtrace.iter().any( |frame| {
        frame.source.as_ref().map( |source| {
            source.ends_with( expected )
        }).unwrap_or( false )
    })
}

impl Analysis {
    fn allocations_from_source< 'a >( &'a self, source: &'a str ) -> impl Iterator< Item = &Allocation > + 'a {
        self.response.allocations.iter().filter( move |alloc| is_from_source( alloc, source ) )
    }
}

fn analyze( name: &str, path: impl AsRef< Path > ) -> Analysis {
    let cwd = repository_root().join( "target" );

    let path = path.as_ref();
    assert_file_exists( path );

    static PORT: AtomicUsize = AtomicUsize::new( 8080 );
    let port = PORT.fetch_add( 1, Ordering::SeqCst );

    let _child = run_in_the_background(
        &cwd,
        cli_path(),
        &[OsString::from( "server" ), path.as_os_str().to_owned(), OsString::from( "--port" ), OsString::from( format!( "{}", port ) )],
        &[("RUST_LOG", "server_core=debug,cli_core=debug,actix_net=info")]
    );

    let start = Instant::now();
    let mut found = false;
    while start.elapsed() < Duration::from_secs( 10 ) {
        thread::sleep( Duration::from_millis( 100 ) );
        if let Some( mut response ) = reqwest::get( &format!( "http://localhost:{}/list", port ) ).ok() {
            assert_eq!( response.status(), StatusCode::OK );
            assert_eq!( *response.headers().get( CONTENT_TYPE ).unwrap(), "application/json" );
            let list: Vec< ResponseMetadata > = serde_json::from_str( &response.text().unwrap() ).unwrap();
            if !list.is_empty() {
                assert_eq!( list[ 0 ].executable.split( "/" ).last().unwrap(), name );
                found = true;
                break;
            }
        }
    }

    assert!( found );

    let mut response = reqwest::get( &format!( "http://localhost:{}/data/last/allocations", port ) ).unwrap();
    assert_eq!( response.status(), StatusCode::OK );
    assert_eq!( *response.headers().get( CONTENT_TYPE ).unwrap(), "application/json" );
    let response: ResponseAllocations = serde_json::from_str( &response.text().unwrap() ).unwrap();

    Analysis { response }
}

fn get_basename( path: &str ) -> &str {
    let index_slash = path.rfind( "/" ).map( |index| index + 1 ).unwrap_or( 0 );
    let index_dot = path.rfind( "." ).unwrap();
    &path[ index_slash..index_dot ]
}

fn compile( source: &str ) {
    let cwd = repository_root().join( "target" );
    let basename = get_basename( source );
    let source_path = PathBuf::from( "../integration-tests/test-programs" ).join( source );
    let source_path = source_path.into_os_string().into_string().unwrap();
    if source.ends_with( ".c" ) {
        run(
            &cwd,
            "gcc",
            &[
                "-fasynchronous-unwind-tables",
                "-O0",
                "-pthread",
                "-ggdb3",
                &source_path,
                "-o",
                basename
            ],
            EMPTY_ENV
        ).assert_success();
    } else {
        run(
            &cwd,
            "g++",
            &[
                "-std=c++11",
                "-fasynchronous-unwind-tables",
                "-O0",
                "-pthread",
                "-ggdb3",
                &source_path,
                "-o",
                basename
            ],
            EMPTY_ENV
        ).assert_success();
    }
}

#[test]
fn test_basic() {
    let cwd = repository_root().join( "target" );

    compile( "basic.c" );

    run(
        &cwd,
        "./basic",
        EMPTY_ARGS,
        &[
            ("LD_PRELOAD", preload_path().into_os_string()),
            ("MEMORY_PROFILER_LOG", "debug".into()),
            ("MEMORY_PROFILER_OUTPUT", "memory-profiling-basic.dat".into())
        ]
    ).assert_success();

    let analysis = analyze( "basic", cwd.join( "memory-profiling-basic.dat" ) );
    let mut iter = analysis.allocations_from_source( "basic.c" );

    let a0 = iter.next().unwrap(); // malloc, leaked
    let a1 = iter.next().unwrap(); // malloc, freed
    let a2 = iter.next().unwrap(); // malloc, freed through realloc
    let a3 = iter.next().unwrap(); // realloc
    let a4 = iter.next().unwrap(); // calloc, freed
    let a5 = iter.next().unwrap(); // posix_memalign, leaked

    assert!( a0.deallocation.is_none() );
    assert!( a1.deallocation.is_some() );
    assert!( a2.deallocation.is_some() );
    assert!( a3.deallocation.is_none() );
    assert!( a4.deallocation.is_none() );
    assert!( a5.deallocation.is_none() );

    assert_eq!( a5.address % 65536, 0 );

    assert!( a0.size < a1.size );
    assert!( a1.size < a2.size );
    assert!( a2.size < a3.size );
    assert!( a3.size < a4.size );
    assert!( a4.size < a5.size );

    assert_eq!( a0.thread, a1.thread );
    assert_eq!( a1.thread, a2.thread );
    assert_eq!( a2.thread, a3.thread );
    assert_eq!( a3.thread, a4.thread );
    assert_eq!( a4.thread, a5.thread );

    assert_eq!( a0.backtrace.last().unwrap().line.unwrap() + 1, a1.backtrace.last().unwrap().line.unwrap() );

    assert_eq!( iter.next(), None );
}

#[test]
fn test_alloc_in_tls() {
    let cwd = repository_root().join( "target" );

    compile( "alloc-in-tls.cpp" );

    run(
        &cwd,
        "./alloc-in-tls",
        EMPTY_ARGS,
        &[
            ("LD_PRELOAD", preload_path().into_os_string()),
            ("MEMORY_PROFILER_LOG", "debug".into()),
            ("MEMORY_PROFILER_OUTPUT", "memory-profiling-alloc-in-tls.dat".into())
        ]
    ).assert_success();

    assert_file_exists( cwd.join( "memory-profiling-alloc-in-tls.dat" ) );
}
