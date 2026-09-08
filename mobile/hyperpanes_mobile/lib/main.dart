import 'package:flutter/material.dart';

import 'src/ui/connect_screen.dart';
import 'src/ui/theme.dart';

void main() {
  runApp(const AvadaApp());
}

class AvadaApp extends StatelessWidget {
  const AvadaApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Avada Terminal',
      theme: buildTheme(),
      debugShowCheckedModeBanner: false,
      home: const ConnectScreen(),
    );
  }
}
