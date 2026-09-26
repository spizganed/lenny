// Auto / Manual (Task 5), as a widget test: the JVM/emulator-free way to cover the UI logic in this sandbox.
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:google_fonts/google_fonts.dart';
import 'package:lenny/core_bindings/lenny_bindings.dart';
import 'package:lenny/state/camera.dart';
import 'package:lenny/ui/components/camera_controls.dart';

void main() {
  GoogleFonts.config.allowRuntimeFetching = false; // no network in tests; fall back to the default font

  const caps = CameraCaps(
    controls: LENNY_CAP_FOCUS | LENNY_CAP_EXPOSURE_COMP | LENNY_CAP_EXPOSURE_LOCK,
    lenses: ['1x'],
    exposure: (min: -2000, max: 2000, step: 500),
  );

  Future<List<CameraCommand>> pump(WidgetTester tester, CameraControls controls) async {
    final sent = <CameraCommand>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: SingleChildScrollView(
          child: ModeControls(caps: caps, controls: controls, onCommand: sent.add, tapHint: 'Tap the preview.'),
        ),
      ),
    ));
    return sent;
  }

  testWidgets('Auto shows no manual controls; Manual reveals them without changing the camera', (tester) async {
    final sent = await pump(tester, const CameraControls());
    expect(find.byType(ExposureSlider), findsNothing);
    expect(find.text('Lock exposure'), findsNothing);
    await tester.tap(find.text('Manual'));
    await tester.pump();
    expect(find.byType(ExposureSlider), findsOneWidget);
    expect(find.text('Lock exposure'), findsOneWidget);
    expect(find.text('Tap the preview.'), findsOneWidget);
    expect(sent, isEmpty); // just showing the controls sends nothing
  });

  testWidgets('a camera with anything manual set is shown as Manual; Auto resets it', (tester) async {
    final sent = await pump(tester, const CameraControls(afMode: 1, exposureEvMilli: 500));
    expect(find.byType(ExposureSlider), findsOneWidget);
    expect(find.textContaining('Focus held'), findsOneWidget);
    await tester.tap(find.text('Auto'));
    await tester.pump();
    expect(sent, [Commands.auto]);
  });

  test('tap-to-focus (focusing or held) is not Auto', () {
    expect(const CameraControls().isAuto, isTrue);
    expect(const CameraControls(afMode: 2).isAuto, isFalse);
    expect(const CameraControls(aeLock: true).isAuto, isFalse);
  });
}
