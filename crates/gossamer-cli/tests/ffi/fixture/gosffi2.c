/* A second fixture library, so one extern block can link two. */
#if defined(_WIN32)
#define GOSFFI2_EXPORT __declspec(dllexport)
#else
#define GOSFFI2_EXPORT
#endif

GOSFFI2_EXPORT int second_library_answer(void) { return 7; }
