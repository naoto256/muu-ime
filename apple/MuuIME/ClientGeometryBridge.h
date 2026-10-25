#pragma once
#import <Cocoa/Cocoa.h>

// Internal geometry bridge, not part of muu_ffi.h or the Rust ABI. The
// candidate attributes fallback reads through this NSException boundary; raw
// values are validated by the caller before they may affect placement. It
// stays after the diagnostics were retired, because it is how the panel is
// still shown in clients whose firstRect fails.
// One attributesForCharacterIndex: answer. `exception` means the client threw;
// `returnedAttributes` means it returned a dictionary at all. lineRect and
// lineHeight start as NaN and orientation as -1, so a value the client did not
// write cannot pass for a real one.
typedef struct {
    BOOL exception;
    BOOL returnedAttributes;
    NSRect lineRect;
    BOOL hasBaseline;
    NSPoint baseline;
    NSInteger orientation;
    double lineHeight;
} MuuClientLineGeometry;

MuuClientLineGeometry MuuClientReadLineGeometry(id _Nonnull client, NSUInteger index);

#if defined(MUU_TEST)
id _Nonnull MuuInputPathThrowingClientForTesting(void);
id _Nonnull MuuInputPathThrowingClientWithTargetForTesting(id _Nonnull target);
id _Nonnull MuuInputPathThrowingClientAtIndexForTesting(id _Nonnull target, NSUInteger index);
#endif
