import 'dart:convert';

import 'package:flutter_hbb/models/cm_file_model.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_test/flutter_test.dart';

class _FakeFFI implements FFI {
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

const _receivedFile = '/Users/test/Downloads/测试 image #1.png';

Future<CmFileModel> _selectedTransfer() async {
  final model = CmFileModel(WeakReference(_FakeFFI()));
  model.updateCurrentClientId(1);
  await Future<void>.delayed(Duration.zero);
  model.currentJobTable.add(CmFileLog()..id = 2);
  return model;
}

String? _updateTransfer(CmFileModel model, Map<String, dynamic> updates) {
  model.onFileTransferLog({
    'transfer': jsonEncode({
      'connId': 1,
      'id': 2,
      'dataSource': _receivedFile,
      'totalSize': 100,
      'finishedSize': 100,
      ...updates,
    }),
  });
  return model.currentJobTable.single.completedReceivePath;
}

void main() {
  test('only a finalized receive exposes the local destination', () async {
    final model = await _selectedTransfer();

    expect(_updateTransfer(model, {}), isNull);
    expect(_updateTransfer(model, {'done': true}), _receivedFile);
  });

  test('the card cannot open sends, failed receives or relative paths',
      () async {
    final model = await _selectedTransfer();

    expect(_updateTransfer(model, {'done': true, 'isRemote': true}), isNull);
    expect(_updateTransfer(model, {'cancel': true}), isNull);
    expect(_updateTransfer(model, {'error': 'skipped'}), isNull);
    expect(_updateTransfer(model, {'error': 'write error'}), isNull);
    expect(
      _updateTransfer(model, {'done': true, 'dataSource': 'image.png'}),
      isNull,
    );
  });
}
