import 'package:flutter_hbb/common/widgets/setting_widgets.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/input_model.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('FPS limit follows the rounded display refresh rate', () {
    expect(fpsLimitForRefreshRate(59.94), 60);
    expect(fpsLimitForRefreshRate(144), 144);
    expect(fpsLimitForRefreshRate(double.nan), kFallbackMaxFps);
  });

  test('custom FPS stays within the display range', () {
    expect(normalizeCustomFps(1, 144), 1);
    expect(normalizeCustomFps(80, 60), 60);
    expect(normalizeCustomFps(0, 60), kDefaultFps);
  });

  test('extreme color shortcut uses the platform primary modifier', () {
    expect(
      isExtremeColorTestShortcut(
        logicalKey: LogicalKeyboardKey.keyC,
        isMac: false,
        controlPressed: true,
        altPressed: true,
        shiftPressed: true,
        commandPressed: false,
      ),
      isTrue,
    );
    expect(
      isExtremeColorTestShortcut(
        logicalKey: LogicalKeyboardKey.keyC,
        isMac: true,
        controlPressed: false,
        altPressed: true,
        shiftPressed: true,
        commandPressed: true,
      ),
      isTrue,
    );
    expect(
      isExtremeColorTestShortcut(
        logicalKey: LogicalKeyboardKey.keyC,
        isMac: false,
        controlPressed: true,
        altPressed: false,
        shiftPressed: true,
        commandPressed: false,
      ),
      isFalse,
    );
  });
}
